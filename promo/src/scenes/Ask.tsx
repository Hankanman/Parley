import { Audio } from "@remotion/media";
import { interpolate, Sequence, useCurrentFrame, useVideoConfig } from "remotion";
import { Feature } from "../components/Feature";
import { Cursor, Panel } from "../components/Ui";
import { c, mono } from "../theme";

const QUESTION = "What did we promise Harbor Logistics?";
// Both facts come from the demo meetings: the Harbor call (01:13) and the roadmap meeting (03:30).
const ANSWER: (string | { cite: string })[] = [
  "Weekly check-ins through the pilot, and a pick-rate report every Monday",
  { cite: "M3 01:13" },
  ". Elena is also setting up two dashboard test sessions with them next week",
  { cite: "M1 03:30" },
  ".",
];
const TYPE = 1.3; // frames per character
const SEND = Math.ceil(QUESTION.length * TYPE) + 14;

export const Ask: React.FC = () => (
  <Feature side="right" headline={"Ask across\n*every meeting.*"} sub="Plain-language questions, answers that cite the exact moment they came from.">
    <Panel width={920} height={660} title="Ask Parley" tilt={12} delay={2}>
      <AskUi />
    </Panel>
    <Sequence from={SEND - 2} durationInFrames={20} layout="none">
      <Audio src="https://remotion.media/mouse-click.wav" volume={0.7} />
    </Sequence>
  </Feature>
);

const AskUi: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const typed = QUESTION.slice(0, Math.floor(Math.max(0, frame - 8) / TYPE));
  const sent = frame >= SEND;
  const answerStart = SEND + Math.round(0.5 * fps);
  let budget = Math.max(0, frame - answerStart) * 3.2; // characters revealed so far
  return (
    <div style={{ position: "absolute", inset: 0, display: "flex", flexDirection: "column" }}>
      <div style={{ flex: 1, padding: "30px 36px", display: "flex", flexDirection: "column", gap: 24 }}>
        {sent ? (
          <div
            style={{
              alignSelf: "flex-end",
              maxWidth: "80%",
              padding: "16px 22px",
              borderRadius: 18,
              background: c.primary,
              fontSize: 27,
              opacity: interpolate(frame, [SEND, SEND + 6], [0, 1], { extrapolateRight: "clamp" }),
              translate: `0px ${interpolate(frame, [SEND, SEND + 8], [30, 0], { extrapolateRight: "clamp" })}px`,
            }}
          >
            {QUESTION}
          </div>
        ) : null}
        {sent && frame < answerStart ? (
          <div style={{ display: "flex", gap: 10, padding: 10 }}>
            {[0, 1, 2].map((i) => (
              <div key={i} style={{ width: 14, height: 14, borderRadius: 7, background: c.fgMuted, opacity: 0.3 + 0.7 * Math.abs(Math.sin(frame * 0.25 + i)) }} />
            ))}
          </div>
        ) : null}
        {frame >= answerStart ? (
          <div style={{ fontSize: 29, lineHeight: 1.6, maxWidth: "92%" }}>
            {ANSWER.map((part, i) => {
              if (typeof part === "string") {
                const shown = part.slice(0, Math.max(0, Math.floor(budget)));
                budget -= part.length;
                return <span key={i}>{shown}</span>;
              }
              const visible = budget > 0;
              const appearedAt = frame - budget / 3.2;
              budget -= 4;
              return visible ? (
                <span
                  key={i}
                  style={{
                    fontFamily: mono,
                    fontSize: 21,
                    padding: "4px 10px",
                    margin: "0 4px",
                    borderRadius: 8,
                    background: `${c.primary}33`,
                    color: c.link,
                    border: `1px solid ${c.primary}`,
                    boxShadow: `0 0 ${interpolate(frame - appearedAt, [0, 10, 25], [0, 24, 0], { extrapolateLeft: "clamp", extrapolateRight: "clamp" })}px ${c.primary}`,
                  }}
                >
                  {part.cite}
                </span>
              ) : null;
            })}
          </div>
        ) : null}
      </div>
      <div style={{ margin: "0 28px 28px", display: "flex", gap: 14, alignItems: "center" }}>
        <div
          style={{
            flex: 1,
            padding: "18px 22px",
            borderRadius: 14,
            border: `1px solid ${sent ? c.border : c.primary}`,
            background: c.raised,
            fontSize: 26,
            color: sent || typed ? c.fg : c.fgMuted,
          }}
        >
          {sent ? <span style={{ color: c.fgMuted }}>Ask across all your meetings…</span> : typed || <span style={{ color: c.fgMuted }}>Ask across all your meetings…</span>}
          {!sent && frame % 30 < 18 ? <span style={{ display: "inline-block", width: 3, height: 28, background: c.link, marginLeft: 3, verticalAlign: -4 }} /> : null}
        </div>
        <div style={{ width: 62, height: 62, borderRadius: 14, background: c.primary, display: "grid", placeItems: "center", fontSize: 28 }}>➤</div>
      </div>
      <Cursor
        path={[
          { frame: 0, x: 560, y: 300 },
          { frame: SEND - 8, x: 850, y: 548 },
          { frame: SEND + 40, x: 640, y: 420 },
        ]}
        clickAt={[SEND]}
      />
    </div>
  );
};
