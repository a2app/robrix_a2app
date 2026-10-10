These small synthetic files contain no account or room data. Transport tests
compare their bytes after download, decryption, and native model encoding.

- `vision-colors.png`: four quadrants, red/green/blue/yellow in reading order.
  The optional live vision test must identify the pixels without filename hints.
- `vision-colors.heic` and `vision-colors.gif`: the same public 256 by 256
  quadrants converted with macOS `sips`. Native HTTP tests verify full HEIC
  bytes reach Gemini under both the specific `image/heic` and generic
  `image/heif` MIME types. They verify GIF reaches OpenAI/Anthropic, and that
  each transport reports unsupported image formats without sending their bytes.

  ```sh
  sips -s format heic vision-colors.png --out vision-colors.heic
  sips -s format gif vision-colors.png --out vision-colors.gif
  ```

- `report.pdf`: a valid one-page PDF using Helvetica, with the text
  `Public fixture report: 42`. Its cross-reference offsets are calculated
  when constructing the five PDF objects.
- `tone.wav`: 0.2 seconds of a 440 Hz tone, mono signed 16-bit PCM at 8000 Hz,
  generated with Python's `wave` module.
- `blue.mp4`: 0.2 seconds of blue frames, 32 by 32 pixels, H.264/yuv420p:

  ```sh
  ffmpeg -f lavfi -i color=c=blue:s=32x32:r=10:d=0.2 -c:v libx264 -pix_fmt yuv420p -movflags +faststart blue.mp4
  ```

The ignored live tests contact an explicitly configured service and may use
its quota. Normal test runs use local HTTP/Matrix fixtures and remain offline.
