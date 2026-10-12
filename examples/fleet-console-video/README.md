# Fleet console video (local, not committed)

Builds the 90 s, 1080p "FluxVM Cloud" explainer from the real console running on a simulated fleet.
Needs Node, ffmpeg and Google Chrome.

```bash
cd examples/fleet-console-video && npm i
FLUXVM_SIM_PORT=18700 ../../scripts/fleet-sim.sh > sim.out 2>&1 &   # wait until it prints the tokens
node capture.mjs                         # console screenshots -> shots/ (dark theme)
# seed: fluxctl fleet prices --set prices.json; fluxctl fleet budget 500 --tenant acme; fluxctl fleet keys create ...
node recap.mjs; node recap3.mjs          # billing/usage with prices, VM inspector
node music.mjs                           # -> music-raw.wav (96 bpm, A minor, 36 bars = 90 s)
ffmpeg -y -i music-raw.wav -t 90 -af "aecho=0.8:0.6:60|180:0.25|0.15,loudnorm=I=-16:TP=-1.5:LRA=9" -ar 48000 music.wav
node render.mjs                          # 2700 JPEG frames -> frames/ (about 10 min)
ffmpeg -framerate 30 -i frames/%05d.jpg -i music.wav -c:v libx264 -crf 24 -pix_fmt yuv420p -c:a aac -b:a 192k -shortest -movflags +faststart out.mp4
```

Scene text and timing are in `scenes.html` (nine 10 s scenes; `window.__seek(t)` makes every frame reproducible).
