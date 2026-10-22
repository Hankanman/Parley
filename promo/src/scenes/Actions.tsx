import { Audio } from "@remotion/media";
import { Easing, interpolate, Sequence, useCurrentFrame, useVideoConfig } from "remotion";
import { Feature } from "../components/Feature";
import { Avatar, Cursor, Panel } from "../components/Ui";
import { c, Person, people } from "../theme";

const ITEMS: { text: string; who: Person; due: string; meeting: string }[] = [
  { text: "Send the first Monday pick-rate report", who: "alex", due: "Monday", meeting: "Customer Call: Harbor Logistics" },
  { text: "Survey Wi-Fi coverage in cold storage", who: "sam", due: "Before go-live", meeting: "Customer Call: Harbor Logistics" },
  { text: "Draft the offline mode spec", who: "alex", due: "Friday", meeting: "Q4 Roadmap Planning" },
  { text: "Break down the queue rework estimate", who: "marcus", due: "Wednesday", meeting: "Q4 Roadmap Planning" },
];
const TICK = 62;

export const Actions: React.FC = () => (
  <Feature headline={"*Nothing*\nslips through."} sub="Action items with owners and due dates, pulled from every meeting into one list.">
    <Panel width={920} height={640} title="Action items" tilt={-12} delay={2}>
      <ActionsUi />
    </Panel>
    <Sequence from={TICK - 2} durationInFrames={20} layout="none">
      <Audio src="https://remotion.media/mouse-click.wav" volume={0.6} />
    </Sequence>
    <Sequence from={TICK + 2} durationInFrames={40} layout="none">
      <Audio src="https://remotion.media/ding.wav" volume={0.35} />
    </Sequence>
  </Feature>
);

const ActionsUi: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const done = frame >= TICK;
  return (
    <div style={{ padding: "26px 30px", display: "flex", flexDirection: "column", gap: 16 }}>
      <div style={{ display: "flex", alignItems: "center", gap: 16, marginBottom: 6 }}>
        <div style={{ fontSize: 30, fontWeight: 700 }}>Open</div>
        <div style={{ fontSize: 24, color: c.fgMuted }}>{done ? 6 : 7} open · {done ? 4 : 3} done</div>
      </div>
      {ITEMS.map((item, i) => {
        const start = 6 + i * 5;
        const enter = interpolate(frame, [start, start + 0.6 * fps], [0, 1], {
          extrapolateLeft: "clamp",
          extrapolateRight: "clamp",
          easing: Easing.bezier(0.16, 1, 0.3, 1),
        });
        const ticked = i === 0 && done;
        const tick = i === 0 ? interpolate(frame, [TICK, TICK + 8], [0, 1], { extrapolateLeft: "clamp", extrapolateRight: "clamp" }) : 0;
        return (
          <div
            key={item.text}
            style={{
              display: "flex",
              alignItems: "center",
              gap: 20,
              padding: "20px 22px",
              borderRadius: 16,
              background: c.raised,
              border: `1px solid ${ticked ? `${c.green}88` : c.border}`,
              opacity: enter * (ticked ? 0.75 : 1),
              translate: `${(1 - enter) * 160}px 0px`,
            }}
          >
            <div
              style={{
                width: 36,
                height: 36,
                borderRadius: 10,
                border: `2px solid ${ticked ? c.green : c.fgMuted}`,
                background: ticked ? c.green : "transparent",
                display: "grid",
                placeItems: "center",
                color: "#fff",
                fontWeight: 800,
                fontSize: 24,
                scale: String(1 + 0.25 * Math.sin(tick * Math.PI)),
              }}
            >
              {ticked ? "✓" : ""}
            </div>
            <div style={{ flex: 1 }}>
              <div style={{ fontSize: 27, position: "relative", display: "inline-block", color: ticked ? c.fgMuted : c.fg }}>
                {item.text}
                <div style={{ position: "absolute", left: 0, top: "52%", height: 2, width: `${tick * 100}%`, background: c.fgMuted }} />
              </div>
              <div style={{ fontSize: 20, color: c.fgMuted, marginTop: 6 }}>{item.meeting}</div>
            </div>
            <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
              <div style={{ textAlign: "right" }}>
                <div style={{ fontSize: 21, color: people[item.who].color }}>{people[item.who].name}</div>
                <div style={{ fontSize: 19, color: c.fgMuted }}>{item.due}</div>
              </div>
              <Avatar who={item.who} size={46} />
            </div>
          </div>
        );
      })}
      <Cursor
        path={[
          { frame: 0, x: 600, y: 520 },
          { frame: TICK - 6, x: 58, y: 118 },
          { frame: TICK + 40, x: 300, y: 300 },
        ]}
        clickAt={[TICK]}
      />
    </div>
  );
};
