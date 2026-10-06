# Performance

## Budgets

| What | Budget |
| --- | --- |
| Launch to window | well under half a second |
| Memory in normal use | under 100–200 MB (separation, later, is exempt) |
| CPU while idle | effectively zero |

## Measuring

```sh
uv run scripts/make_test_audio.py --long   # once
apps/macos/build.sh
scripts/bench.sh --save before             # ... make a change, rebuild ...
scripts/bench.sh --save after
diff bench-results/before.txt bench-results/after.txt
```

`scripts/bench.sh` measures three things: the core's decode-and-summarise
speed and peak memory per format; the app's launch (time from process
creation to the window, and to a file's waveform being on screen); and the
app at rest with a file open. It runs the app against a throwaway data
folder, so it never touches your settings or recent files. Launch numbers are
medians, because any single launch is noisy.

## Current results

Apple M2, 8 GB RAM, macOS 27.0.1, release build (5.0 MB app bundle).

Core, decode + waveform:

| File (4 minutes unless noted) | Time | Speed | Peak memory |
| --- | --- | --- | --- |
| WAV | 122 ms | 1967× real time | 7.3 MB |
| FLAC | 168 ms | 1431× | 7.7 MB |
| MP3 | 199 ms | 1208× | 7.6 MB |
| AAC (M4A) | 277 ms | 868× | 8.1 MB |
| MP3, 60 minutes | 2967 ms | 1213× | 19.4 MB |

App launch, median of 5, milliseconds since the process was created:

| Scenario | Window | Waveform on screen | Peak memory |
| --- | --- | --- | --- |
| No file | 177 | — | 39.7 MB |
| 4 min MP3, first time | 236 | 504 | 75.9 MB |
| 4 min MP3, opened before | 240 | 240 | 63.4 MB |
| 60 min MP3, first time | 388 | 3625 | 90.8 MB |
| 60 min MP3, opened before | 216 | 216 | 75.3 MB |

At rest with a file open: 67 MB (4-minute file) and 72 MB (60-minute file),
0.1 % CPU.

## What made the difference

Measured with the same script before and after (first run saved as
`01-baseline`, second as `02-fast-open`):

| | Before | After |
| --- | --- | --- |
| Previously opened 4 min file on screen | 441 ms | 240 ms |
| Previously opened 60 min file on screen | 443 ms | 216 ms |
| At rest, 4 min file | 89 MB | 67 MB |
| At rest, 60 min file | 111 MB | 72 MB |
| Peak, 60 min file (cached) | 134 MB | 75 MB |

1. A cached waveform is loaded before the open call returns, and the app
   waits up to 60 ms for an open to finish, so a file opened before appears
   in the first frame instead of after a start screen and a second layout.
2. A file named at launch is opened before any UI is built.
3. Registering the file with the system's recent-documents list is deferred
   until after the window is drawn.
4. The waveform cache is read in small chunks straight into its final
   buffer rather than through a second full-size buffer.
5. The player no longer opens the file a second time just to validate it.

## Known costs

- The first open of a long file is bound by decoding: about 3 seconds per
  hour of MP3. It shows progress and is cached afterwards. Splitting the
  decode across cores is the obvious next step if this matters.
- The waveform pyramid costs about 13 MB per hour of audio while a file is
  open.
- The timeline's drawing surface is about 13 MB at a 1180 × 700 window on a
  Retina display and grows with the window.
