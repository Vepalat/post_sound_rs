use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use soketto::handshake::ServerResponse;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::compat::TokioAsyncReadCompatExt;

#[derive(Parser)]
#[command(version, about="send mic voice through websocket", long_about = None)]
struct ArgParser {
    url: String,
    #[arg(long, default_value_t = 5.0)]
    sec: f32,
    #[arg(long)]
    no_vad: bool,
    #[arg(long, default_value_t = 2.0)]
    silence: f32,
    #[arg(long)]
    keepprompt: bool,
    #[arg(long)]
    outfile: Option<PathBuf>,
}

#[derive(serde::Serialize)]
struct Config {
    vad: bool,
    secs: f32,
    min_silence_duration_s: f32,
    keepprompt: bool,
    vadconfig: Vadconfig,
    codec_format: String,
    language: String,
}

#[derive(serde::Serialize)]
struct Vadconfig {}

#[tokio::main]
async fn main() -> Result<()> {
    let args = ArgParser::parse();
    let config = Config {
        vad: !args.no_vad,
        secs: args.sec,
        min_silence_duration_s: args.silence,
        keepprompt: args.keepprompt,
        vadconfig: Vadconfig {},
        codec_format: "opus".to_owned(),
        language: "ja".to_owned(),
    };

    let parsedurl = url::Url::parse(&args.url)?;
    let host = parsedurl.host_str().unwrap();
    let port = parsedurl.port();
    let hostport = format!(
        "{}{}",
        host,
        port.map(|x| ":".to_owned() + &x.to_string())
            .unwrap_or("".to_owned())
    );
    let socket = tokio::net::TcpStream::connect(&hostport).await?;
    let mut client = soketto::handshake::Client::new(socket.compat(), &hostport, parsedurl.path());
    let (mut sender, mut receiver) = match client.handshake().await? {
        ServerResponse::Accepted { .. } => client.into_builder().finish(),
        ServerResponse::Redirect {
            status_code: _,
            location: _,
        } => unimplemented!("follow location URL"),
        ServerResponse::Rejected { status_code: _ } => unimplemented!("handle failure"),
    };

    sender.send_text(serde_json::to_string(&config)?).await?;

    // define mic device
    let device = cpal::default_host()
        .default_input_device()
        .expect("mic device not found");
    println!("{}", device.name()?);

    let mut channel_set = std::collections::HashSet::new();
    for i in device.supported_input_configs()? {
        channel_set.insert(i.channels());
    }
    let selected_channel = channel_set
        .into_iter()
        .min()
        .expect("supported_input_configs is empty");
    let device_config = cpal::StreamConfig {
        channels: selected_channel,
        ..device.default_input_config()?.config()
    };

    // ffmpeg process
    let samplerate_str: &str = &device_config.sample_rate.0.to_string();
    let selected_channel_str: &str = &selected_channel.to_string();
    let mut ffmpeg_proc = tokio::process::Command::new("ffmpeg")
        .args([
            "-f",
            "f32le",
            "-ar",
            samplerate_str,
            "-ac",
            selected_channel_str,
            "-i",
            "pipe:",
            "-ac",
            "1",
            "-f",
            "opus",
            "pipe:",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    // send ffmpeg task
    let (tx, rx) = std::sync::mpsc::channel();
    println!("{:?}", device_config);
    let stream = device.build_input_stream(
        &device_config,
        move |data: &[f32], _info| {
            let (_, b, _) = unsafe { data.align_to() };
            let b = b.to_owned();
            let _ = tx.send(b);
        },
        |e| {
            eprintln!("device stream: an error occurred on stream: {}", e);
        },
        Some(std::time::Duration::from_secs(1)),
    )?;
    stream.play()?;

    // ffmpeg_sender
    let (tx_ffmpeg_sender, mut rx_ffmpeg_sender) = tokio::sync::oneshot::channel();
    let ffmpeg_sender = tokio::spawn(async move {
        let mut stdin = ffmpeg_proc.stdin.take().unwrap();
        loop {
            match rx_ffmpeg_sender.try_recv() {
                Ok(_) => break,
                Err(e) => match e {
                    tokio::sync::oneshot::error::TryRecvError::Empty => {},
                    tokio::sync::oneshot::error::TryRecvError::Closed => panic!("{:?}", e),
                }
            };
            let Ok(data) = tokio::task::block_in_place(|| rx.recv()) else {
                println!("ffmpeg_sender: mic stream stop");
                break;
            };
            let res = stdin.write_all(&data).await;
            if let Err(e) = res {
                println!("ffmpeg_sender: {:?}", e);
                break;
            }
        }
        stdin.shutdown().await.unwrap();
    });

    // recv ffmpeg and send websocket task
    let (tx_ffmpeg_recv_ws_send, mut rx_ffmpeg_recv_ws_send) = tokio::sync::mpsc::channel(1);
    let ffmpeg_recv_ws_send = tokio::spawn(async move {
        // ffmpeg_recver
        let mut stdout = ffmpeg_proc.stdout.take().unwrap();
        let mut v = vec![0; 4096];
        loop {
            let result = tokio::select! {
                result = stdout.read(&mut v[..]) => {
                    result.unwrap()
                }
                rxres = rx_ffmpeg_recv_ws_send.recv() => {
                    match rxres {
                        Some(_) => {
                            sender.close().await.expect("sender close failed");
                            break;
                        },
                        None => panic!("rx_ffmpeg_recv_ws_send.recv()")
                    }
                }
            };
            match result {
                0 => {
                    println!("ffmpeg_recv_ws_send: ffmpeg stdout read size is 0");
                    sender.close().await.unwrap();
                    break;
                }
                _ => {
                    let res = sender.send_binary(&v[..result]).await;
                    if let Err(e) = res {
                        match e {
                            soketto::connection::Error::Closed => {
                                println!("ffmpeg_recv_ws_send: websocket closed");
                                break;
                            }
                            _ => panic!("{:?}", e),
                        }
                    }
                }
            }
        }
    });

    let (tx_ws_recv, mut rx_ws_recv) = tokio::sync::mpsc::channel(1);
    let ws_recv = tokio::spawn(async move {
        // recv_task
        // recv websocket and write file
        let mut file = if let Some(outfile) = args.outfile {
            println!("write to {}", outfile.to_str().unwrap());
            if !outfile.exists() {
                tokio::fs::File::create(&outfile).await.unwrap();
            }
            let f = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&outfile)
                .await
                .unwrap();
            Some(f)
        } else {
            None
        };

        // write file "%Y-%m-%d-%H:%M:%S start"
        let s = {
            chrono::Local::now()
                .format("%Y-%m-%d-%H:%M:%S start\n")
                .to_string()
        };
        if let Some(ref mut f) = file {
            f.write_all(s.as_bytes()).await.unwrap();
            f.flush().await.unwrap();
        }

        let mut message = Vec::new();
        loop {
            message.clear();
            let r = tokio::select! {
                r = receiver.receive_data(&mut message) => {
                    r
                }
                rxres = rx_ws_recv.recv() => {
                    match rxres {
                        Some(_) => break,
                        None => panic!("rx_ws_recv.recv()")
                    }
                }
            };
            match r {
                Ok(soketto::Data::Text(n)) => {
                    debug_assert_eq!(n, message.len());
                    let messagetext = std::str::from_utf8(&message).unwrap();
                    let mut dec: Vec<String> = serde_json::from_str(messagetext).unwrap();
                    debug_assert_eq!(dec.len(), 3);

                    // remove "ご視聴ありがとうございました"
                    dec[0] = dec[0].replace("ご視聴ありがとうございました", "");

                    // if text is not none
                    // write "{text[1]} -> {text[2]}: {text[0]}"
                    if !dec[0].is_empty() {
                        let s = format!("{} -> {}: {}\n", dec[1], dec[2], dec[0]);
                        print!("{s}");
                        if let Some(ref mut f) = file {
                            f.write_all(s.as_bytes()).await.unwrap();
                            f.flush().await.unwrap();
                        }
                    }
                }
                Ok(_) => {
                    panic!();
                    // unreachable if server send textonly data. but, the case rely server implementation.
                    // unreachable!();
                }
                Err(e) => {
                    println!("ws_recv: {:?}", e);
                    break;
                }
            }
        }
    });

    let ctrlc = tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        let _ = tx_ffmpeg_sender.send(());
        let _ = tx_ffmpeg_recv_ws_send.send(()).await;
        let _ = tx_ws_recv.send(()).await;
        println!("ctrlc");
    });

    let res = tokio::try_join! {
        ffmpeg_sender,
        ffmpeg_recv_ws_send,
        ws_recv,
    };

    {
        let res = stream.pause();
        if let Err(e) = res {
            println!("stream.pause: {:?}", e);
        }
    }

    if let Err(e) = res {
        println!("join: {:?}, {:?}", e, e.to_string());
        Err(e)?;
    }

    if !ctrlc.is_finished() {
        return Err(anyhow::anyhow!("error exit".to_owned()));
    }

    println!("exit");
    Ok(())
}

#[cfg(test)]
mod tests {
    use cpal::traits::{DeviceTrait, HostTrait};

    #[test]
    fn case1() {
        let device = cpal::default_host().default_input_device().unwrap();
        println!("{:?}", device.name());
        println!("{:?}", device.default_input_config().unwrap());
        let v = device
            .supported_input_configs()
            .unwrap()
            .collect::<Vec<_>>();
        for i in v {
            println!("{i:?}");
        }
    }
}
