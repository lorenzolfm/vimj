# vimj

Spotify-style Mix for music you keep yourself, in the terminal: paste a YouTube link to download a
song, then let vimj order your songs and blend each one into the next like a DJ.

- **Downloads**: paste a link (YouTube, YouTube Music, SoundCloud, Bandcamp and the
  [other sites yt-dlp knows](https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md)).
  Playlists work too. The best audio stream is kept as served, without re-encoding.
- **Real transitions**: both songs play at once for 1–16 bars. The incoming song is time-stretched
  onto the outgoing tempo (pitch kept) so the beats lock. The same five ingredients as Spotify's Mix
  shape the handover, with every style it offers: volume, EQ, filter, effects (reverb, echo, delay)
  and looping. Presets as in Spotify: Fade, Rise, Blend, Wave, Melt, Slam, Dissolve, Surge,
  Afterglow. Or draw your own volume, EQ, filter and effect curves, point by point.
- **Automatic for every pair**: entry on the incoming song's first downbeat, exit on the outgoing
  song's last phrase boundary, preset picked from how well the two match.
- **Smart reorder** (`s`) arranges a playlist, or the whole library, for the smoothest run.
- **Five best next songs** (`t`) from your whole library, by key (Camelot wheel), tempo and energy.
- **Analysis** of each song on arrival: tempo, beat grid with downbeats, key, energy, and loudness,
  which evens out levels between songs.
- **Vim-style keys**: counts, `dd`/`yy`/`p`, visual line mode, `:` commands, `/` search.
  The mouse works as well.

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
./target/release/vimj
```

or `nix run` / `nix profile install .`, which wraps ffmpeg and yt-dlp in.

**Debian / Ubuntu**: `sudo apt install pkg-config libasound2-dev libclang-dev ffmpeg`, then
`pipx install yt-dlp`, then `cargo build --release`.

**macOS**: `brew install ffmpeg yt-dlp`, then `cargo build --release`.

`vimj` starts the daemon, `vimjd`, in the background when it isn't running. Music keeps playing
after you quit with `q`; `Q` quits and stops it. If something else answers on the port, say a
`vimjd` from an older build, `vimj` says so and stops rather than talk to it.

## Getting songs in

Paste a link anywhere (or `:add <link>`), and `gd` shows the downloads. Songs land in `~/Music/vimj`,
named `Artist - Title [id].ext`. Any audio file you drop in that folder is picked up as well at
start or with `:rescan`.

If YouTube answers with *"Sign in to confirm you're not a bot"*, let yt-dlp use your browser's
YouTube login: `:cookies firefox` (or `chrome`, `brave`, `edge`, `safari`…); `:cookies off` undoes it.

YouTube's best audio for most videos is Opus at about 130 kbps. vimj keeps that stream untouched;
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
| `Enter` `space` | play from here, pause |
| `>` `<` | next, previous song |
| `]` `[` `}` `{` | seek 10 s, 30 s |
| `+` `-` | volume |

Every playlist mixes: each song blends into the next.

Commands: `:add <link>`, `:new <name>`, `:rename <name>`, `:delete`, `:reorder`,
`:title <text>` and `:artist <text>` for the song under the cursor, `:vol 80`, `:seek 1:30`,
`:cookies <browser>`, `:rescan`, `:clear` (finished downloads), `:q`, `:q!`, `:help`. `Tab` completes.

### Mouse

| | |
| --- | --- |
| click | put the cursor on a song or download; open a playlist from the sidebar |
| double-click | play from that song |
| drag | select songs, as `V` does |
| wheel | scroll the list; over `vol`, change the volume |
| click INTO NEXT | edit that transition |
| click the waveform, `▶`, the title | seek there, pause, go to what's playing |

In the transition editor, click a field or the `‹` `›` beside its value, or turn the wheel over it.
Drag a waveform sideways to move that song's cue point a beat at a time. Click a preset by name, or
`Auto`. Double-click a curve's field to draw it; then click a point on the waveform to pick it, drag
it (it snaps to beats and the middle), or double-click the curve to add one.

Hold shift to select text in the terminal as usual.

### The transition editor

`e` opens it: the outgoing song (A) above the incoming one (B) on one timeline, as in Spotify, the
transition window shaded across both. Each song's curves are drawn over its waveform, one colour per
kind: volume blue (full at the top), EQ yellow (flat in the middle, cuts go down; the fainter lines
are the mids and highs), filter pink (open in the middle, a high-pass closes upwards and a low-pass
downwards), effects white (more send, higher). Bar lines run above A and below B. When A loops, `↺`
marks each jump back and the repeats show dimmer.

| | |
| --- | --- |
| `j` `k` | pick a field: preset, length, sync, A out, B in, volume, EQ, filter, effects, looping |
| `h` `l` | change it; on A out / B in, move by a beat |
| `H` `L`, `w` `b` | move by a bar, by an eight-bar phrase |
| `,` `.` | nudge by 10 ms, to line beats up by ear |
| `1`–`9`, `0` | Fade, Rise, Blend, Wave, Melt, Slam, Dissolve, Surge, Afterglow; back to automatic |
| `c` | draw the chosen field's curves by hand (volume, EQ, filter, effects) |
| `s`, `+` `-` | sync on / off, longer / shorter |
| `u` `C-r` | undo, redo |
| `space` | loop the transition while you edit; it plays your edits before you save them |
| `[` `]` | previous / next transition in the playlist |
| `:w` | save; `[+]` in the header means there are edits to save |
| `:wq` `:x` | save and close |
| `Esc` `q` `:q` | close; with unsaved edits it asks you to `:w` or `:q!` first |
| `:q!`, `:e!` | close throwing the edits away; throw them away and keep editing |

The options are Spotify's, in its order:

| | |
| --- | --- |
| Volume | Smooth crossfade, Overlap, Fade in fade out, Cut in fade out, Fade in cut out, Center cut, Crossfade, Fade in fast out |
| EQ | Center bass swap, End bass swap, Start bass swap, 3-band fade, Quick bass cut, Long bass cut, Start fade out, Bass fade out, None |
| Filter | Low pass filter out, Low pass filter in, Low pass filter in low pass filter out, Low pass filter in high pass filter out, High pass filter out, High pass filter in, High pass filter in high pass filter out, High pass filter in low pass filter out, High-pass filter half out, Noise out end, None |
| Effects | Reverb out center, Reverb cut end, Reverb out end, Delay ½ cut end, Delay ¾ cut end, Echo ½ cut end, Echo ½ out end, Echo ¾ cut end, Echo ¾ out end, Echo 1 cut end, Echo 1 out end, None |
| Looping | 1, 2, 4, 8 or 16 beat loop, Loop halving, None |

"In" filters open on B, "out" ones close on A. Effects and loops go on A: an effect that goes "out"
rings on after the window, one that "cuts" stops where it ends; echoes repeat, delays repeat once,
every ½, ¾ or 1 beat. A loop replays A from the start of the window until it ends.

#### Drawing curves

`c` puts points on the field's curves, right on the waveforms, as Spotify's Customise does, and dims
the other curves. A song the style leaves alone (no filter on B, say) has no curve. Picking another
style, or a preset, puts the style's curves back.

| | |
| --- | --- |
| `w` `b`, `0` `$` | next / previous point, first / last |
| `h` `l`, `H` `L` | move the point by a beat, by a bar; `,` `.` nudge it |
| `j` `k`, `J` `K` | value down / up by 5%, by 25% |
| `o` | add a point after this one |
| `x` | delete it (the first and last points stay, and only move up and down) |
| `s` | ease the segment after it in and out, or run it straight |
| `<` `>` | bend that segment down / up |
| `Tab` | the other song's curve |
| `R` | back to the style's curve for this song |
| `Esc` `c` | back to the fields |

Transitions belong to a pair of songs, so a pair tuned once sounds the same in every playlist.

## How it works, and its limits

Songs are decoded with ffmpeg and played through your sound card (ALSA, which PipeWire and
PulseAudio provide). Two decks run at once during a transition. The incoming deck goes through
[Signalsmith Stretch](https://signalsmith-audio.co.uk/code/stretch/) so its tempo matches without its
pitch moving, then eases back to its own tempo over 16 beats. Each deck has a three-band isolator EQ,
a filter, a noise sweep, a tempo-synced echo and a reverb, and can loop a stretch of its song; the
master has a soft clipper.

- The beat grid assumes a steady tempo. Live recordings that drift won't stay locked, and an unusual
  rhythm can put the grid half a beat off. Move B by a beat, or nudge it with `,` `.`.
- Key detection is a best guess from the song's harmony, so treat it as a hint.
- Songs too far apart in tempo (more than 10%) aren't stretched; their automatic transition is short.
- Spotify's app names its styles but its servers hold their exact shapes and what Dissolve, Surge
  and Afterglow combine. vimj's are read from the names, so they may differ from Spotify's in detail.
  Every curve shows in the editor, and can be redrawn.

## Layout

```
crates/core    vimj-core   keys, compatibility, smart reorder, transition curves, wire types
crates/audio   vimj-audio  ffmpeg decoding, analysis, decks, the mix engine, sound output
crates/daemon  vimjd       library, playlists, yt-dlp downloads, HTTP API + server-sent events
crates/tui     vimj        ratatui frontend
```

Frontends talk to `vimjd` over HTTP on `127.0.0.1:8787`; `vimj_core::protocol` has the requests,
responses and the events streamed from `GET /api/events`, so a web or desktop frontend can reuse it.
`GET /api/status` names the `protocol` the daemon speaks (`vimj_core::protocol::PROTOCOL`); talk
only to a daemon whose protocol is your own.

State lives in `~/.config/vimj/config.toml` (music folder, tool paths, browser for cookies, volume)
and `~/.local/share/vimj/` (the library, waveforms, `vimjd.log`). If `vimjd` can't read the library,
say one a newer vimj wrote, it stops and says why in `vimjd.log` rather than start over empty. Older
builds don't check, so before `vimjd` first saves a library from an older vimj it keeps a copy as it
was, `library.v0.json`: put that back as `library.json` before running an older build.

`cargo run --release -p vimj-audio --example analyze -- FILE…` prints what analysis finds in files;
`--example render -- FROM TO OUT.wav [PRESET] [BARS]` renders a transition to listen to. `vimjd
--silent` (or `VIMJ_AUDIO=null`) plays into nothing, for testing.

```sh
cargo test --workspace
```
