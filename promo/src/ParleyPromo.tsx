import { Audio } from "@remotion/media";
import { linearTiming, springTiming, TransitionPresentation, TransitionSeries } from "@remotion/transitions";
import { fade } from "@remotion/transitions/fade";
import { slide } from "@remotion/transitions/slide";
import { wipe } from "@remotion/transitions/wipe";
import { AbsoluteFill, interpolate, Sequence } from "remotion";
import music from "./assets/music.mp3";
import { Actions } from "./scenes/Actions";
import { Ask } from "./scenes/Ask";
import { Hook, LogoReveal } from "./scenes/Intro";
import { Outro, Privacy, RealApp } from "./scenes/Outro";
import { Record } from "./scenes/Record";
import { Speakers } from "./scenes/Speakers";
import { Summary } from "./scenes/Summary";
import { Transcript } from "./scenes/Transcript";

const FPS = 30;
const T = 14; // transition length, frames

// [name, scene, seconds]
const SCENES: [string, React.FC, number][] = [
  ["Hook", Hook, 4.0],
  ["Logo", LogoReveal, 2.0],
  ["Record", Record, 4.6],
  ["Transcript", Transcript, 5.6],
  ["Summary", Summary, 5.2],
  ["Ask", Ask, 5.6],
  ["Actions", Actions, 4.4],
  ["Speakers", Speakers, 4.2],
  ["Real app", RealApp, 3.8],
  ["Privacy", Privacy, 3.6],
  ["Outro", Outro, 4.0],
];

const frames = (s: number) => Math.round(s * FPS);

export const PROMO_DURATION = SCENES.reduce((sum, [, , s]) => sum + frames(s), 0) - T * (SCENES.length - 1);

type AnyPresentation = TransitionPresentation<Record<string, unknown>>;
const presentations = [
  () => fade(),
  () => slide({ direction: "from-right" }),
  () => slide({ direction: "from-bottom" }),
  () => wipe({ direction: "from-left" }),
] as (() => AnyPresentation)[];

export const ParleyPromo: React.FC = () => {
  // frame at which each cut (transition midpoint) happens, for the whooshes
  let at = 0;
  const cuts = SCENES.slice(0, -1).map(([, , s]) => {
    at += frames(s) - T;
    return at;
  });
  return (
    <AbsoluteFill style={{ backgroundColor: "#0B0E14" }}>
      <TransitionSeries>
        {SCENES.flatMap(([name, Scene, s], i) => {
          const items = [
            <TransitionSeries.Sequence key={name} name={name} durationInFrames={frames(s)}>
              <Scene />
            </TransitionSeries.Sequence>,
          ];
          if (i < SCENES.length - 1) {
            items.push(
              <TransitionSeries.Transition
                key={`${name}-t`}
                presentation={presentations[i < 1 ? 0 : i % presentations.length]()}
                timing={i < 1 ? linearTiming({ durationInFrames: T }) : springTiming({ config: { damping: 200 }, durationInFrames: T })}
              />,
            );
          }
          return items;
        })}
      </TransitionSeries>
      {/* original track, synthesised by music/make-music.py at 120 BPM (one beat = 15 frames) */}
      <Audio
        src={music}
        volume={(f) => interpolate(f, [PROMO_DURATION - 45, PROMO_DURATION], [0.55, 0], { extrapolateLeft: "clamp", extrapolateRight: "clamp" })}
      />
      {cuts.slice(1).map((cut) => (
        <Sequence key={cut} from={cut - 4} durationInFrames={30} layout="none">
          <Audio src="https://remotion.media/whoosh.wav" volume={0.25} />
        </Sequence>
      ))}
    </AbsoluteFill>
  );
};
