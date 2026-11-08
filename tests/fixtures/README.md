# Synthetic streaming fixture

`live.aac` is an ADTS AAC-LC stream generated entirely from a synthetic 997 Hz
sine wave (48 kHz, stereo, nominal 96 kbit/s, two seconds). It contains no
recorded music, station URLs, external media, or third-party copyrighted audio.
The test loops its complete ADTS frames indefinitely, adding ICY metadata on
the wire; the fixture itself contains no ICY framing.

Generated with FFmpeg 9.0.2 using:

```sh
ffmpeg -hide_banner -loglevel error \
  -f lavfi -i sine=frequency=997:sample_rate=48000 -t 2 \
  -ac 2 -c:a aac -b:a 96k -f adts tests/fixtures/live.aac
```

FFmpeg is needed only to regenerate this fixture, not to run the test. Encoder
versions can produce different valid bytes. Keep the checked-in fixture below
40 KiB. Run the regression with:

```sh
cargo build
python3 tests/streaming.py
# Or test another already-built binary:
RADIOD_BINARY=/absolute/path/to/radiod python3 tests/streaming.py
```

The test needs only Python's standard library, the binary, and its GStreamer
HTTP/ICY/AAC plugins. Both its HTTP server and MPD connection bind/connect only
to `127.0.0.1`; no station list or outbound HTTP access is used. A synchronized
null sink avoids physical audio devices. This tests actual AAC/ICY decoding,
startup, status/metadata, switching, cancellation, and cleanup. It does not
reproduce HTTP/2 transport bugs, assert audible output, or replace a dedicated
buffer-starvation/clock test.
