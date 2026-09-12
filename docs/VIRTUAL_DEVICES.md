# Virtual audio device setup (real user setup step - not solved by this code)

ClearNAI needs **two distinct virtual audio endpoints** at once, playing two
different roles:

1. **Virtual microphone target.** The engine renders its fully-processed mic
   signal *into* a render endpoint. Other apps then pick the *paired capture
   endpoint* as their microphone and hear the cleaned-up signal.
2. **Virtual speaker source.** Other apps render *their* output into some
   *other* capture-paired endpoint. The engine captures from that endpoint,
   cleans it up, and forwards it to your real hardware speakers.

## Why one VB-Cable install is not enough

A single VB-Audio Virtual Cable installation provides exactly **one**
render/capture pair (by default named "CABLE Input" and "CABLE Output").
If you tried to use that one pair for both roles above, an app that captured
"CABLE Output" as its microphone and another app that rendered into "CABLE
Input" as its speaker would collide on the *same* pair - there is no way to
tell those two data streams apart on a single cable.

**You must install two independent virtual cable instances** - for example:

- Two separate installs of a VB-Cable-family product, each exposing its own
  differently-named pair (e.g. via VB-Audio's paid multi-cable products), or
- VB-Audio's **"VB-CABLE A+B"** bundle, which installs two independently
  named pairs ("CABLE-A Input"/"CABLE-A Output" and "CABLE-B
  Input"/"CABLE-B Output") in one package.

Then, in ClearNAI's GUI:

- Set **"Virtual mic target"** to one pair's render endpoint (e.g. "CABLE-A
  Input").
- Set **"Virtual speaker source"** to the *other* pair's capture endpoint
  (e.g. "CABLE-B Output").
- In other applications, select the corresponding paired endpoints: "CABLE-A
  Output" as your microphone, and route your desired app output to "CABLE-B
  Input" as its speaker.

## What the code does and does not do about this

- `audio-io::devices::find_vb_cable_endpoints()` looks for endpoints whose
  friendly name contains "CABLE Input"/"CABLE Output" (VB-Cable's *default*
  single-pair naming). ClearNAI's startup logic
  (`app/src/engine.rs::default_virtual_mic_target` /
  `default_virtual_speaker_source`) only **auto-selects** a device for a role
  when exactly one matching endpoint exists for that role - it never guesses
  between multiple candidates, and it never assumes the same physical cable
  pair is safe to use for both roles simultaneously.
- If your system only has one virtual cable pair installed, at most **one**
  of the two roles can be auto-selected (or neither, if the naming doesn't
  match); the other role's dropdown will show "(not selected)" and that half
  of the pipeline will not start (this is treated as a startup warning, not
  a crash - see the GUI's warning banner).
- The GUI's two device pickers (populated from
  `audio_io::devices::list_render_devices()` /
  `list_capture_devices()`) let you assign **any** enumerated device to
  either role manually. Nothing in the code prevents you from picking the
  same physical cable's two ends for both roles - if you do, you will get
  exactly the collision described above. This is a user configuration
  mistake the code cannot detect for you (it cannot know which endpoints
  belong to the same physical cable beyond name-matching heuristics), so
  install a second, independent cable instance rather than trying to reuse
  one pair for both roles.
- Device selections now take effect **live**: picking a different device in
  any of the four pickers ("Physical microphone", "Virtual mic target",
  "Virtual speaker source", "Monitor output") stops the affected WASAPI
  capture/render thread and starts a fresh one on the newly-selected device,
  reconnecting it to a new ring buffer - no full app restart required. This
  is a deliberate, narrow exception to the "never rebuild the pipeline live"
  rule: that rule is specifically about the 6 DSP bypass toggles (Noise/BVC/
  Studio/AEC on/off), which must be glitch-free every single frame; a device
  change is a different, much rarer, user-initiated action, and a brief
  reconnect blip (tens to hundreds of ms while the old device closes and the
  new one opens) is expected and normal here, the same way switching output
  devices works in any other real-time audio application. See
  `engine::LiveAudioSwitcher` for the implementation. Expect a short gap in
  audio during the switch itself, not a glitch-free transition.

This is a genuine, unresolved-by-software user setup requirement, not a bug
to fix later: no single installer or piece of code here can conjure a second
independent virtual cable pair out of one VB-Cable install.

## Using the virtual mic in a BPO/call-center dialer or softphone

No app-specific integration is needed. ClearNAI forwards its cleaned-up mic
signal into an ordinary WASAPI capture endpoint ("CABLE Output" or
"CABLE-A/B Output"), so **any softphone/dialer that lets you choose a
microphone device from a dropdown** (Avaya, Genesys, Five9, RingCentral,
Zoiper, X-Lite, Windows' own Teams/Zoom, etc.) picks it up the same way it
would a physical mic - open that app's audio/device settings and select the
"CABLE Output" (or "CABLE-A Output") endpoint as its input device. If the
dialer is a browser-based/WebRTC softphone running inside Chrome/Edge,
select it as the microphone in the browser's site permission prompt or
`chrome://settings/content/microphone` instead of the OS-wide default.
