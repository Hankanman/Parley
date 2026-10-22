# Parley promo video

A [Remotion](https://www.remotion.dev/) project that renders the ~40 s promo
(`docs/images/promo.mp4`, shown on meetparley.app). The feature scenes rebuild
Parley's UI as animated React components using the app's own theme colours
and the fictional demo data from `parley-core/examples/demo/seed.sql`; the
real-app montage uses the screenshots in `docs/images/` (Remotion's public
folder is pointed there in `remotion.config.ts`).

```bash
npm i
npm run dev                       # Remotion Studio preview
npx remotion render ParleyPromo ../docs/images/promo.mp4 --crf=20
npx remotion still ParleyPromo ../docs/images/promo-poster.png --frame=560
```

The backing track (`src/assets/music.mp3`) is original, synthesised by
`music/make-music.py` (numpy + scipy + ffmpeg) at 120 BPM so one beat is
15 frames. Sound effects are Remotion's hosted samples.

If Studio dies with `EMFILE: too many open files, watch`, the inotify
instance limit is exhausted: `sudo sysctl fs.inotify.max_user_instances=1024`.

Remotion is free for individuals and teams of up to three; see
https://www.remotion.dev/license.
