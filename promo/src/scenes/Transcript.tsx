import { interpolate, useCurrentFrame, useVideoConfig } from "remotion";
import { Feature } from "../components/Feature";
import { Avatar, Panel } from "../components/Ui";
import { c, mono, Person, people } from "../theme";

// From the demo "Customer Call: Harbor Logistics" (parley-core/examples/demo/seed.sql)
const SEGMENTS: { who: Person; me?: boolean; at: string; text: string }[] = [
  { who: "sam", at: "00:00", text: "Thanks for making time. We have forty units arriving at the Rotterdam site next week." },
  { who: "alex", me: true, at: "00:21", text: "Great. What does success look like for you at the end of the pilot?" },
  { who: "sam", at: "00:44", text: "Pick rates up by fifteen percent, and the night shift able to resolve alerts without calling an engineer." },
  { who: "alex", me: true, at: "01:13", text: "That's very achievable. We'll share a pick-rate report every Monday." },
];
const WORD = 2.2; // frames per word
const GAP = 8;

export const Transcript: React.FC = () => (
  <Feature side="right" headline={"Transcribed live.\n*On your device.*"} sub="Whisper, GPU-accelerated on NVIDIA, AMD and Intel. Every speaker labelled as they talk.">
    <Panel width={920} height={680} title="Customer Call: Harbor Logistics" tilt={12} delay={2}>
      <TranscriptUi />
    </Panel>
  </Feature>
);

const TranscriptUi: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  let t = 0.5 * fps;
  const starts = SEGMENTS.map((s) => {
    const start = t;
    t += s.text.split(" ").length * WORD + GAP;
    return start;
  });
  return (
    <div style={{ position: "absolute", inset: 0, padding: "30px 34px", display: "flex", flexDirection: "column", justifyContent: "flex-end", gap: 26 }}>
      {SEGMENTS.map((s, i) => {
        const local = frame - starts[i];
        if (local < 0) return null;
        const words = s.text.split(" ");
        const shown = Math.min(words.length, Math.floor(local / WORD) + 1);
        const typing = shown < words.length;
        return (
          <div
            key={i}
            style={{
              display: "flex",
              gap: 18,
              opacity: interpolate(local, [0, 6], [0, 1], { extrapolateRight: "clamp" }),
              translate: `0px ${interpolate(local, [0, 8], [20, 0], { extrapolateRight: "clamp" })}px`,
            }}
          >
            <Avatar who={s.who} size={52} />
            <div style={{ flex: 1 }}>
              <div style={{ display: "flex", alignItems: "center", gap: 12, marginBottom: 6 }}>
                <span style={{ fontSize: 24, fontWeight: 600, color: people[s.who].color }}>{s.me ? "Me" : people[s.who].name}</span>
                <span style={{ fontFamily: mono, fontSize: 20, color: c.fgMuted }}>[{s.at}]</span>
                <span style={{ width: 9, height: 9, borderRadius: 5, background: c.green }} />
              </div>
              <div style={{ fontSize: 30, lineHeight: 1.4 }}>
                {words.slice(0, shown).join(" ")}
                {typing ? <span style={{ display: "inline-block", width: 3, height: 30, marginLeft: 4, background: c.link, verticalAlign: -4 }} /> : null}
              </div>
            </div>
          </div>
        );
      })}
    </div>
  );
};
