import { AbsoluteFill, Easing, interpolate, useCurrentFrame, useVideoConfig } from "remotion";
import { Backdrop } from "../components/Backdrop";
import { Headline, Sub } from "../components/Headline";
import { Avatar } from "../components/Ui";
import { c, noise, Person, people } from "../theme";

const ORDER: Person[] = ["priya", "marcus", "alex", "elena", "sam"];

/** Five voices; each lights up and gets its name as it "speaks". */
export const Speakers: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  return (
    <Backdrop hue={c.magenta}>
      <AbsoluteFill style={{ alignItems: "center", paddingTop: 120 }}>
        <Headline text={"Knows *who said what.*"} size={104} align="center" />
        <Sub text="Name a voice once and Parley recognises it next time. Voice profiles never leave your disk." align="center" delay={14} />
      </AbsoluteFill>
      <AbsoluteFill style={{ flexDirection: "row", justifyContent: "center", alignItems: "flex-end", gap: 70, paddingBottom: 170 }}>
        {ORDER.map((who, i) => {
          const start = 14 + i * 9;
          const pop = interpolate(frame, [start, start + 0.5 * fps], [0, 1], {
            extrapolateLeft: "clamp",
            extrapolateRight: "clamp",
            easing: Easing.spring({ damping: 11, stiffness: 160 }),
          });
          const talking = Math.max(0, Math.sin((frame - start) * 0.11 - i * 1.1));
          const named = interpolate(frame, [start + 14, start + 22], [0, 1], { extrapolateLeft: "clamp", extrapolateRight: "clamp" });
          return (
            <div key={who} style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 22, scale: String(pop) }}>
              <div style={{ display: "flex", gap: 5, alignItems: "center", height: 70 }}>
                {Array.from({ length: 9 }, (_, b) => (
                  <div
                    key={b}
                    style={{
                      width: 8,
                      height: 8 + 58 * talking * noise(i * 5 + b, frame * 1.6),
                      borderRadius: 4,
                      background: people[who].color,
                      opacity: 0.4 + 0.6 * talking,
                    }}
                  />
                ))}
              </div>
              <div style={{ borderRadius: 999, boxShadow: `0 0 0 ${6 + talking * 14}px ${people[who].color}${talking > 0.4 ? "44" : "18"}` }}>
                <Avatar who={who} size={150} />
              </div>
              <div style={{ textAlign: "center", opacity: named, translate: `0px ${(1 - named) * 12}px` }}>
                <div style={{ fontSize: 32, fontWeight: 600, color: c.fg }}>{who === "alex" ? "You" : people[who].name}</div>
                <div style={{ fontSize: 22, color: c.green, marginTop: 6 }}>✓ Recognised</div>
              </div>
            </div>
          );
        })}
      </AbsoluteFill>
    </Backdrop>
  );
};
