# Sample clips

A show's listing usually comes with a short MP3 taken from one track, so people can
hear the recording before they download it. Sites cap its size, commonly at 1 MB. This
covers `lh sample` and the `sample` screen in `lh-tui`, which both make that clip.

## 1. What it does

It takes one track (FLAC or WAV), a start time, a length and an MP3 mode, and writes
`<stem>.sample.mp3`.

* **Beside the show folder, not inside it.** A file inside the folder would end up in the
  checksums and the torrent. That is the same reason `torrent create` writes
  `<folder>.torrent` beside the folder. `-o` puts it somewhere else.
* **The size limit is checked twice.** For CBR, the size is known before encoding
  (`bitrate × length`, plus a few frames of overhead), so a clip that cannot fit is refused
  up front, and the error says how long a clip would fit. For VBR the size is only an
  estimate, so the encoded file is measured. If it is over the limit it is discarded, not
  committed (Principle 1: never a file under the real name that fails its own contract).
* **The default clip:** the longest one that fits the limit, capped at 30 s, centred in
  the track. A sample from the start of a track is usually tuning and crowd noise.

## 2. Modes

| mode  | lame        | ffmpeg (libmp3lame) | size estimate        |
|-------|-------------|---------------------|----------------------|
| 320…128 | `-b N --cbr` | `-b:a Nk`         | exact, before encoding |
| V0    | `-V 0`      | `-q:a 0`            | ~245 kbps, checked after |
| V2    | `-V 2`      | `-q:a 2`            | ~190 kbps, checked after |

The default is 256 kbps CBR, because 30 s of it fits 1 MB.

## 3. Encoder

Principle 3: anything that produces a file goes through a reference tool, and Principle 2
records which tool that was. The reference MP3 encoder is `lame`, so `lame` is preferred
(`LH_LAME` overrides discovery). If `lame` is not found, `ffmpeg` is used (`LH_FFMPEG`),
because it links the same libmp3lame and is often already installed. The provenance
record names whichever one ran, with its version, hash and argv. Neither tool is required
for the rest of the app. If neither is found, the error names both.

Neither tool has a portable "start here, for this long" that works the same way in both,
so the excerpt is cut in-process: the FLAC is decoded (up to the end of the clip; claxon
cannot seek) or the WAV's range is read, and it is written to a temporary WAV that the
encoder reads. The clip's title, artist and album tags go into the MP3's ID3 tags.

## 4. The screen

`lh-tui sample <folder>` (or a file, which preselects that track), and `p` in the
workspace. Tracks on the left, with title and length. The fields are on the right:
start, length, format (←/→) and limit. Below them are the clip span, the estimated size
against the limit (red when over) and the longest clip that fits. `Enter` encodes, with a
gauge while the FLAC is decoded and a spinner while the encoder runs. `q`/`Esc` stops an
encode in progress. An output that already exists is only replaced with `F`.
