# Performance

## Budgets

| What | Budget |
| --- | --- |
| Launch to window | well under half a second |
| Memory in normal use | under 100–200 MB (isolating vocals is exempt) |
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
creation to the window, to a file's waveform being on screen, and to its
pitch being on screen); and the app at rest with a file open. It runs the app against a throwaway data
folder, so it never touches your settings or recent files. Launch numbers are
medians, because any single launch is noisy.

## Current results

Apple A18 Pro (fanless), 8 GB RAM, macOS 27.0, release build of 0.6.0
(25 MB app bundle; it was 5 MB before ONNX Runtime was linked in).

Core, decode + waveform:

| File (4 minutes unless noted) | Time | Speed | Peak memory |
| --- | --- | --- | --- |
| WAV | 93 ms | 2594× real time | 7.7 MB |
| FLAC | 134 ms | 1789× | 8.1 MB |
| MP3 | 175 ms | 1375× | 7.9 MB |
| AAC (M4A) | 230 ms | 1044× | 8.5 MB |
| MP3, 60 minutes | 2623 ms | 1372× | 19.8 MB |

Core, pitch analysis (`cargo run --release --example analyse`):

| File | Pitch tracking | Notes and indicators |
| --- | --- | --- |
| WAV, 4 minutes | 0.39 s (618× real time) | 24 ms |
| MP3, 60 minutes | 7.3 s (492×) | 315 ms |

Core, vocal isolation of a 4-minute MP3 on five threads
(`cargo run --release --example isolate_vocals`):

| Model | Time | Speed | Peak memory |
| --- | --- | --- | --- |
| Kim Vocal 2 | 74 s | 3.3× real time | 2.3 GB |
| KUIELab MDX-Net B | 25 s | 9.7× | 1.2 GB |

App launch, median of 5, milliseconds since the process was created:

| Scenario | Window | Waveform on screen | Pitch on screen | Peak memory |
| --- | --- | --- | --- | --- |
| No file | 122 | — | — | 25.3 MB |
| 4 min MP3, first time | 198 | 359 | 776 | 99.4 MB |
| 4 min MP3, opened before | 194 | 194 | 194 | 74.5 MB |
| 60 min MP3, first time | 200 | 2980 | 9938 | 150.2 MB |
| 60 min MP3, opened before | 208 | 208 | 543 | 114.0 MB |

At rest with a file open and analysed: 81 MB (4-minute file) and 105 MB
(60-minute file), 0.1 % CPU.

For comparison, 0.1.0 measured 67 MB and 72 MB at rest and 177–240 ms to the
window, on an Apple M2; the two machines differ, so only the rough size of
the change is meaningful: the pitch analysis of an open file costs in the
region of 15 MB for four minutes of audio and 30 MB for an hour.

## What made the difference

Measured on an Apple M2 with the same script before and after (first run
saved as `01-baseline`, second as `02-fast-open`):

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

- Isolating vocals is the one thing that is slow and heavy: roughly a third
  of the recording's length and 2.3 GB of memory with the large model, on
  the CPU. It runs in the background, shows progress, can be cancelled, and
  its result is kept. Running the models on the graphics processor has not
  been tried.
- The first pitch analysis of a long file takes about 7 seconds per hour of
  audio, across four threads, after the waveform is already on screen. It is cached, and for
  recordings over ten minutes the notes are derived off the open path, which
  is why an hour-long file's pitch appears a third of a second after its
  waveform.

- The first open of a long file is bound by decoding: about 3 seconds per
  hour of MP3. It shows progress and is cached afterwards. Splitting the
  decode across cores is the obvious next step if this matters.
- The waveform pyramid costs about 13 MB per hour of audio while a file is
  open.
- The timeline's drawing surface is about 13 MB at a 1180 × 700 window on a
  Retina display and grows with the window.
