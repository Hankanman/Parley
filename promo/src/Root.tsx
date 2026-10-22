import { Composition } from "remotion";
import { PROMO_DURATION, ParleyPromo } from "./ParleyPromo";

export const RemotionRoot: React.FC = () => {
  return (
    <Composition
      id="ParleyPromo"
      component={ParleyPromo}
      durationInFrames={PROMO_DURATION}
      fps={30}
      width={1920}
      height={1080}
    />
  );
};
