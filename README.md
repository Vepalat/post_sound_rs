# about
whisper_batchstreamのクライアント

post_soundと比べて、標準入力からの音声入力ができない

post_sound rust version.
# build
1. rustをインストールする。
2. `cargo build -r`
# run
run `target/release/post_sound_rs`

```
send mic voice through websocket

Usage: post_sound_rs [OPTIONS] <URL>

Arguments:
  <URL>  

Options:
      --sec <SEC>          [default: 5]
      --no-vad             
      --silence <SILENCE>  [default: 2]
      --keepprompt         
      --outfile <OUTFILE>  
  -h, --help               Print help
  -V, --version            Print version
```

## example
whisper_batchstreamを同一PCで起動している状態で、
`post_sound_rs --sec 5 ws://localhost:9000/ws --outfile out.txt`

## only impl
- mic mode
- opus codec
- concat text always false
- language ja

# args
- url must be only hostname, no scheme
- secs float
- no-vad
- silence float
- keepprompt
- outfile path

# requirements
- ffmpeg

# use lib
- clap argparser
- cpal record from mic
- tokio async lib
- soketto

