import React from 'react';
import {Composition} from 'remotion';
import {FPS} from './theme';
import {PROMO_DUR, Promo} from './Promo';
import {HERO_DUR, Hero} from './Hero';

export const Root: React.FC = () => (
  <>
    <Composition
      id="Promo"
      component={Promo}
      durationInFrames={PROMO_DUR}
      fps={FPS}
      width={1920}
      height={1080}
      defaultProps={{layout: 'landscape' as const}}
    />
    <Composition
      id="PromoVertical"
      component={Promo}
      durationInFrames={PROMO_DUR}
      fps={FPS}
      width={1080}
      height={1920}
      defaultProps={{layout: 'portrait' as const}}
    />
    <Composition id="Hero" component={Hero} durationInFrames={HERO_DUR} fps={FPS} width={1920} height={1080} />
  </>
);
