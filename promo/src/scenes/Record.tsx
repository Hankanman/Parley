import { Audio } from "@remotion/media";
import { interpolate, Sequence, useCurrentFrame, useVideoConfig } from "remotion";
import { Feature } from "../components/Feature";
import { Cursor, Panel } from "../components/Ui";
import { c, mono, noise } from "../theme";

const CLICK = 34;

export const Record: React.FC = () => (
  <Feature headline={"One click.\n*Both sides* of the call."} sub="Your mic and the call audio, straight from PipeWire. No bot joins your meeting.">
    <Panel width={900} height={640} title="Parley — Record" tilt={-12} delay={2}>
      <RecordUi />
    </Panel>
    <Sequence from={CLICK - 2} durationInFrames={20} layout="none">
      <Audio src="https://remotion.media/mouse-click.wav" volume={0.7} />
    </Sequence>
  </Feature>
);

const RecordUi: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const on = frame >= CLICK;
  const since = Math.max(0, frame - CLICK);
  const seconds = Math.floor((since / fps) * 7); // the clock runs fast for the edit
  const level = interpolate(since, [0, 0.6 * fps], [0, 1], { extrapolateRight: "clamp" });
  const pulse = 0.55 + 0.45 * Math.abs(Math.sin(frame * 0.12));
  return (
    <div style={{ padding: 34, display: "flex", flexDirection: "column", gap: 26 }}>
      <div style={{ display: "flex", alignItems: "center", gap: 18 }}>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 10,
            padding: "8px 18px",
            borderRadius: 999,
            fontSize: 24,
            fontWeight: 600,
            background: on ? `${c.red}22` : `${c.green}1F`,
            color: on ? c.red : c.green,
          }}
        >
          <div style={{ width: 12, height: 12, borderRadius: 6, background: on ? c.red : c.green, opacity: on ? pulse : 1 }} />
          {on ? "Recording" : "Ready"}
        </div>
        <div style={{ fontSize: 30, fontWeight: 600 }}>Harbor Logistics: Pilot Kickoff</div>
        <div style={{ marginLeft: "auto", fontFamily: mono, fontSize: 44, color: on ? c.fg : c.fgMuted }}>
          {String(Math.floor(seconds / 60)).padStart(2, "0")}:{String(seconds % 60).padStart(2, "0")}
        </div>
      </div>
      <div
        style={{
          alignSelf: "flex-start",
          padding: "16px 30px",
          borderRadius: 14,
          fontSize: 28,
          fontWeight: 600,
          background: on ? "transparent" : c.primary,
          border: `2px solid ${on ? c.red : c.primary}`,
          color: on ? c.red : "#fff",
          scale: String(interpolate(frame, [CLICK - 3, CLICK, CLICK + 6], [1, 0.94, 1], { extrapolateLeft: "clamp", extrapolateRight: "clamp" })),
        }}
      >
        {on ? "■  Stop recording" : "●  Start recording"}
      </div>
      <Meter label="Microphone" sub="You" seed={1} level={level} color={c.link} />
      <Meter label="System audio" sub="Everyone else on the call" seed={7} level={level} color={c.green} />
      <Cursor
        path={[
          { frame: 0, x: 700, y: 520 },
          { frame: CLICK - 6, x: 200, y: 150 },
          { frame: CLICK + 30, x: 230, y: 168 },
          { frame: CLICK + 70, x: 760, y: 470 },
        ]}
        clickAt={[CLICK]}
      />
    </div>
  );
};

const Meter: React.FC<{ label: string; sub: string; seed: number; level: number; color: string }> = ({ label, sub, seed, level, color }) => {
  const frame = useCurrentFrame();
  const bars = 44;
  return (
    <div style={{ background: c.raised, border: `1px solid ${c.border}`, borderRadius: 16, padding: "20px 24px" }}>
      <div style={{ display: "flex", alignItems: "baseline", gap: 14, marginBottom: 14 }}>
        <div style={{ fontSize: 26, fontWeight: 600 }}>{label}</div>
        <div style={{ fontSize: 21, color: c.fgMuted }}>{sub}</div>
      </div>
      <div style={{ display: "flex", alignItems: "center", gap: 6, height: 84 }}>
        {Array.from({ length: bars }, (_, i) => {
          // speech-like envelope: bursts that travel across the bars
          const env = Math.max(0, Math.sin(frame * 0.09 + seed - i * 0.12));
          const h = 6 + 74 * level * Math.min(1, noise(seed * 13 + i, frame * 1.4) * (0.35 + env));
          return <div key={i} style={{ flex: 1, height: h, borderRadius: 6, background: color, opacity: 0.35 + 0.65 * (h / 80) }} />;
        })}
      </div>
    </div>
  );
};
