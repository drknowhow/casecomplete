import {loadFont as loadGrotesk} from '@remotion/google-fonts/SpaceGrotesk';
import {loadFont as loadPlexSans} from '@remotion/google-fonts/IBMPlexSans';
import {loadFont as loadPlexMono} from '@remotion/google-fonts/IBMPlexMono';

// Fonts load from Google Fonts at render time; Remotion holds every frame until they are ready.
const grotesk = loadGrotesk('normal', {weights: ['500', '600', '700'], subsets: ['latin']});
const plexSans = loadPlexSans('normal', {weights: ['400', '500', '600'], subsets: ['latin']});
const plexMono = loadPlexMono('normal', {weights: ['400', '500', '600'], subsets: ['latin']});

export const F = {
  display: `${grotesk.fontFamily}, sans-serif`,
  body: `${plexSans.fontFamily}, sans-serif`,
  mono: `${plexMono.fontFamily}, monospace`,
};

export const C = {
  ink: '#17191C',
  ghost: '#9A9DA3',
  paper: '#F7F7F5',
  night: '#121418',
  tile: '#1A1D22',
  blue: '#3A7BD5',
  green: '#2E8B57',
  greenText: '#4FB37D',
  orange: '#F0A35E',
  red: '#E5484D',
  hair: 'rgba(247,247,245,0.10)',
  hairStrong: 'rgba(247,247,245,0.18)',
  dim: 'rgba(247,247,245,0.62)',
  paperHair: 'rgba(23,25,28,0.10)',
  paperDim: 'rgba(23,25,28,0.55)',
};

export const FPS = 30;

export type Layout = 'landscape' | 'portrait';
