import { AbsoluteFill } from "remotion";
import { Backdrop } from "./Backdrop";
import { Headline, Sub } from "./Headline";

/** Caption on one side, an animated UI panel on the other. */
export const Feature: React.FC<{
  headline: string;
  sub: string;
  side?: "left" | "right";
  hue?: string;
  children: React.ReactNode;
}> = ({ headline, sub, side = "left", hue, children }) => {
  const caption = (
    <div style={{ width: 760, flexShrink: 0 }}>
      <Headline text={headline} size={100} delay={4} />
      <Sub text={sub} delay={16} />
    </div>
  );
  return (
    <Backdrop hue={hue}>
      <AbsoluteFill
        style={{
          flexDirection: "row",
          alignItems: "center",
          justifyContent: "space-between",
          padding: "0 120px",
          gap: 60,
        }}
      >
        {side === "left" ? caption : null}
        <div style={{ position: "relative" }}>{children}</div>
        {side === "right" ? caption : null}
      </AbsoluteFill>
    </Backdrop>
  );
};
