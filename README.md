# dejay

Spotify-style Mix for music you keep yourself, in the terminal: paste a YouTube link to download a
song, then let dejay order your songs and blend each one into the next like a DJ.

- **Downloads**: paste a link (YouTube, YouTube Music, SoundCloud, Bandcamp and the
  [other sites yt-dlp knows](https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md)).
  Playlists work too. The best audio stream is kept as served, without re-encoding.
- **Real transitions**: both songs play at once for 1–16 bars. The incoming song is time-stretched
  onto the outgoing tempo (pitch kept) so the beats lock, and volume, EQ and effect curves shape the
  handover. Presets as in Spotify's Mix: Fade, Rise, Blend, Wave, Melt, Slam, or your own curves.
- **Automatic for every pair**: entry on the incoming song's first downbeat, exit on the outgoing
  song's last phrase boundary, preset picked from how well the two match.
- **Smart reorder** (`s`) arranges a playlist, or the whole library, for the smoothest run.
- **Five best next songs** (`t`) from your whole library, by key (Camelot wheel), tempo and energy.
- **Analysis** of each song on arrival: tempo, beat grid with downbeats, key, energy, and loudness,
  which evens out levels between songs.
- **Vim-style keys**: counts, `dd`/`yy`/`p`, visual line mode, `:` commands, `/` search.

Only download what you have the right to.

## Install

You need `ffmpeg`, a recent `yt-dlp` (YouTube changes often, so keep it updated), a Rust toolchain
(1.88 or newer), and on Linux the ALSA headers and libclang.

**NixOS / Nix**: the flake has a dev shell and a package. Flakes only see files git tracks, so add
them first:

```sh
git add -A
nix develop            # pkg-config, ALSA, libclang, ffmpeg, yt-dlp
cargo build --release
./target/release/dejay
```

or `nix run` / `nix profile install .`, which wraps ffmpeg and yt-dlp in.

**Debian / Ubuntu**: `sudo apt install pkg-config libasound2-dev libclang-dev ffmpeg`, then
`pipx install yt-dlp`, then `cargo build --release`.

**macOS**: `brew install ffmpeg yt-dlp`, then `cargo build --release`.

`dejay` starts the daemon, `dejayd`, in the background when it isn't running. Music keeps playing
after you quit with `q`; `Q` quits and stops it.

## Getting songs in

Paste a link anywhere (or `:add <link>`), and `gd` shows the downloads. Songs land in `~/Music/dejay`,
named `Artist - Title [id].ext`. Any audio file you drop in that folder is picked up as well at
start or with `:rescan`.

If YouTube answers with *"Sign in to confirm you're not a bot"*, let yt-dlp use your browser's
YouTube login: `:cookies firefox` (or `chrome`, `brave`, `edge`, `safari`…); `:cookies off` undoes it.

YouTube's best audio for most videos is Opus at about 130 kbps. dejay keeps that stream untouched;
converting it to FLAC or 320 kbps MP3 would only make the file bigger.

## Keys

| Move | |
| --- | --- |
| `j` `k`, `5j` | down, up, with a count |
| `gg` `G` `12G` | top, bottom, line 12 |
| `C-d` `C-u` `C-f` `C-b` | half page, page |
| `h` `l` `Tab` | sidebar, list |
| `/` `n` `N` | search titles and artists |
| `gl` `gd` `gc` | library, downloads, what's playing |

| Edit | |
| --- | --- |
| `dd` `x` `3dd` | remove from the playlist (in the library: delete the song and its file, after asking) |
| `yy` `p` `P` | yank, put after / before (`dd` then `p` moves) |
| `J` `K` | move down / up |
| `V` | select lines, then `d` `y` `J` `K` `a` |
| `a` | add to a playlist |
| `u` `C-r` | undo, redo |

| Mix and play | |
| --- | --- |
| `t` | the five songs that flow best out of this one; Enter puts it next |
| `e` | edit the transition into the next song |
| `s` | smart reorder |
| `m` | mix on / off (off plays songs back to back) |
| `Enter` `space` | play from here, pause |
| `>` `<` | next, previous song |
| `]` `[` `}` `{` | seek 10 s, 30 s |
| `+` `-` | volume |

Commands: `:add <link>`, `:new <name>`, `:rename <name>`, `:delete`, `:mix on|off`, `:reorder`,
`:title <text>` and `:artist <text>` for the song under the cursor, `:vol 80`, `:seek 1:30`,
`:cookies <browser>`, `:rescan`, `:clear` (finished downloads), `:q`, `:q!`, `:help`. `Tab` completes.

### The transition editor

`e` opens it: the outgoing song (A) above the incoming one (B) on one timeline, with bar lines
(downbeats bright) and the volume, bass and effect curves underneath.

| | |
| --- | --- |
| `j` `k` | pick a field: preset, length, sync, A out, B in, volume, EQ, effect |
| `h` `l` | change it; on A out / B in, move by a beat |
| `H` `L`, `w` `b` | move by a bar, by an eight-bar phrase |
| `,` `.` | nudge by 10 ms, to line beats up by ear |
| `1`–`6`, `0` | Fade, Rise, Blend, Wave, Melt, Slam; back to automatic |
| `s`, `+` `-` | sync on / off, longer / shorter |
| `space` | loop the transition while you edit |
| `[` `]` | previous / next transition in the playlist |
| `Esc` | close (edits save as you go) |

Transitions belong to a pair of songs, so a pair tuned once sounds the same in every playlist.

## How it works, and its limits

Songs are decoded with ffmpeg and played through your sound card (ALSA, which PipeWire and
PulseAudio provide). Two decks run at once during a transition. The incoming deck goes through
[Signalsmith Stretch](https://signalsmith-audio.co.uk/code/stretch/) so its tempo matches without its
pitch moving, then eases back to its own tempo over 16 beats. Each deck has a three-band isolator EQ,
a filter, and a tempo-synced echo; the master has a soft clipper.

- The beat grid assumes a steady tempo. Live recordings that drift won't stay locked, and an unusual
  rhythm can put the grid half a beat off. Move B by a beat, or nudge it with `,` `.`.
- Key detection is a best guess from the song's harmony, so treat it as a hint.
- Songs too far apart in tempo (more than 10%) aren't stretched; their automatic transition is short.

## Layout

```
crates/core    dejay-core   keys, compatibility, smart reorder, transition curves, wire types
crates/audio   dejay-audio  ffmpeg decoding, analysis, decks, the mix engine, sound output
crates/daemon  dejayd       library, playlists, yt-dlp downloads, HTTP API + server-sent events
crates/tui     dejay        ratatui frontend
```

Frontends talk to `dejayd` over HTTP on `127.0.0.1:8787`; `dejay_core::protocol` has the requests,
responses and the events streamed from `GET /api/events`, so a web or desktop frontend can reuse it.

State lives in `~/.config/dejay/config.toml` (music folder, tool paths, browser for cookies, volume)
and `~/.local/share/dejay/` (the library, waveforms, `dejayd.log`).

`cargo run --release -p dejay-audio --example analyze -- FILE…` prints what analysis finds in files;
`--example render -- FROM TO OUT.wav [PRESET] [BARS]` renders a transition to listen to. `dejayd
--silent` (or `DEJAY_AUDIO=null`) plays into nothing, for testing.

```sh
cargo test --workspace
```
