import { interpolate, useCurrentFrame, useVideoConfig } from "remotion";
import { Feature } from "../components/Feature";
import { fadeUp, Panel } from "../components/Ui";
import { c, mono } from "../theme";

const SUMMARY =
  "Harbor Logistics receives 40 units at its Rotterdam site next week. Success means a 15% higher pick rate and a night shift that can resolve alerts without an engineer.";
const DECISIONS = ["Weekly check-ins through the pilot", "Pick-rate report every Monday", "Survey cold-storage Wi-Fi before go-live"];
const CHAPTERS: [string, string][] = [
  ["00:00", "Pilot scope"],
  ["00:44", "Success criteria"],
  ["01:35", "Cold-storage Wi-Fi"],
];

export const Summary: React.FC = () => (
  <Feature headline={"Written up\nthe moment\n*you hang up.*"} sub="Summary, decisions and chapters, from templates for stand-ups, client calls, retros and more.">
    <Panel width={920} height={700} title="Summary · Standard Meeting Notes" tilt={-12} delay={2}>
      <SummaryUi />
    </Panel>
  </Feature>
);

const SummaryUi: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const ready = Math.round(0.9 * fps);
  const generating = frame < ready;
  const chars = Math.floor(Math.max(0, frame - ready) * 7);
  const textDone = ready + SUMMARY.length / 7;
  return (
    <div style={{ padding: "30px 36px", display: "flex", flexDirection: "column", gap: 22 }}>
      {generating ? <Generating /> : null}
      {!generating ? (
        <>
          <div style={{ fontSize: 30, fontWeight: 700 }}>Summary</div>
          <div style={{ fontSize: 27, lineHeight: 1.5, color: c.fg, minHeight: 160 }}>{SUMMARY.slice(0, chars)}</div>
          <div style={{ fontSize: 30, fontWeight: 700, ...fadeUp(frame, textDone, fps) }}>Key decisions</div>
          {DECISIONS.map((d, i) => (
            <div key={d} style={{ display: "flex", alignItems: "center", gap: 16, fontSize: 26, ...fadeUp(frame, textDone + 6 + i * 6, fps) }}>
              <div style={{ width: 30, height: 30, borderRadius: 15, background: c.green, display: "grid", placeItems: "center", color: "#fff", fontSize: 20, fontWeight: 800 }}>✓</div>
              {d}
            </div>
          ))}
          <div style={{ display: "flex", gap: 14, marginTop: 8, ...fadeUp(frame, textDone + 26, fps) }}>
            {CHAPTERS.map(([at, name]) => (
              <div key={at} style={{ padding: "10px 16px", borderRadius: 12, background: c.raised, border: `1px solid ${c.border}`, fontSize: 22 }}>
                <span style={{ fontFamily: mono, color: c.link, marginRight: 10 }}>{at}</span>
                {name}
              </div>
            ))}
          </div>
        </>
      ) : null}
    </div>
  );
};

const Generating: React.FC = () => {
  const frame = useCurrentFrame();
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 20 }}>
      <div style={{ fontSize: 26, color: c.link, fontWeight: 600 }}>✦ Generating summary…</div>
      {[0.95, 0.88, 0.92, 0.6, 0.8, 0.7].map((w, i) => (
        <div
          key={i}
          style={{
            height: 26,
            width: `${w * 100}%`,
            borderRadius: 8,
            background: `linear-gradient(90deg, ${c.muted} 0%, #243049 ${interpolate((frame * 3 + i * 12) % 120, [0, 120], [-20, 120])}%, ${c.muted} 100%)`,
          }}
        />
      ))}
    </div>
  );
};
