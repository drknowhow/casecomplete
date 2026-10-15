import React from 'react';
import {AbsoluteFill, Easing, interpolate, spring, useCurrentFrame, useVideoConfig} from 'remotion';
import {C, F, type Layout} from '../theme';
import {docAt, fixTimes, typeWords, type Part} from '../lib/typing';
import {DocText} from '../components/text';
import {Stat, TabKey, Win, pressDepth, pulseAt} from '../components/ui';
import {Kicker} from '../components/brand';

const clamp = {extrapolateLeft: 'clamp', extrapolateRight: 'clamp'} as const;
const FPC = 2.2;

// ---- Timeline (frames from the start of the scene) ----
const T0 = 26;
const s1 = typeWords(
  T0,
  'I hahve>have been stuck on thsi>this for a whiel>while becuase>because teh>the rulee>rule is easty>easy to usee>use',
  'with-email',
  FPC,
);
const G1 = {text: 'but easy to get wrong.', showT: s1.end + 5, acceptT: s1.end + 27};
const G2 = {text: ' Could you take a look before Friday?', showT: G1.acceptT + 8, acceptT: G1.acceptT + 30};
const SW = G2.acceptT + 24; // switch surfaces
const SW_LEN = 16;
const s3 = typeWords(SW + SW_LEN, 'fix teh>the tst>test', 'with-term', FPC);
const G3 = {text: 'that fails on empty input', showT: s3.end + 5, acceptT: s3.end + 25};
const DONE = G3.acceptT;
export const WITH_DUR = DONE + 80;

const EMAIL: Part[] = [s1.part, {kind: 'ghost', ghost: G1}, {kind: 'ghost', ghost: G2}];
const TERM: Part[] = [s3.part, {kind: 'ghost', ghost: G3}];
const FIXES = [...fixTimes(EMAIL), ...fixTimes(TERM)];
const ACCEPTS = [G1.acceptT, G2.acceptT, G3.acceptT];

/** Active typing time; the clock pauses while the camera moves between apps. */
const activeFrames = (f: number) =>
  Math.max(0, Math.min(f, G2.acceptT) - T0) + Math.max(0, Math.min(f, DONE) - (SW + SW_LEN));

const Field: React.FC<{label: string; children: React.ReactNode}> = ({label, children}) => (
  <div
    style={{
      display: 'flex',
      alignItems: 'center',
      gap: 22,
      padding: '16px 40px',
      borderBottom: `2px solid ${C.paperHair}`,
      fontFamily: F.body,
      fontSize: 26,
    }}
  >
    <span style={{color: C.ghost, width: 96}}>{label}</span>
    <span style={{color: C.ink}}>{children}</span>
  </div>
);

export const Email: React.FC<{
  width: number;
  fontSize: number;
  frame: number;
  lines: number;
  parts?: Part[];
  start?: number;
  docOpacity?: number;
}> = ({width, fontSize, frame, lines, parts = EMAIL, start = T0, docOpacity = 1}) => {
  const state = docAt(parts, frame);
  return (
    <Win title="New message" width={width}>
      <Field label="To">
        <span style={{background: 'rgba(58,123,213,0.12)', color: C.ink, borderRadius: 999, padding: '4px 16px'}}>
          Maya Chen
        </span>
      </Field>
      <Field label="Subject">Lint rule question</Field>
      <div
        style={{
          padding: '26px 40px 30px',
          fontFamily: F.body,
          fontSize,
          lineHeight: 1.42,
          color: C.ink,
          minHeight: fontSize * 1.42 * lines,
          boxSizing: 'content-box',
        }}
      >
        <div>Hi Maya,</div>
        <div style={{marginTop: fontSize * 0.45, opacity: docOpacity}}>
          <DocText state={state} frame={frame} ink={C.ink} ghost={C.ghost} caret={C.ink} hideCaret={frame < start - 10} />
        </div>
      </div>
    </Win>
  );
};

const Terminal: React.FC<{width: number; fontSize: number; frame: number; minHeight: number}> = ({
  width,
  fontSize,
  frame,
  minHeight,
}) => {
  const state = docAt(TERM, frame);
  return (
    <Win dark title="claude  ~  C:\code\parser" titleFont={F.mono} width={width}>
      <div
        style={{
          padding: '26px 34px 30px',
          fontFamily: F.mono,
          fontSize,
          lineHeight: 1.5,
          color: C.paper,
          minHeight,
          boxSizing: 'border-box',
        }}
      >
        <div style={{color: C.dim}}>PS C:\code\parser&gt; claude</div>
        <div style={{color: C.dim, marginTop: fontSize * 0.6}}>&gt; run the tests</div>
        <div style={{whiteSpace: 'pre'}}>
          <span style={{color: C.dim}}>{'  41 passed, '}</span>
          <span style={{color: C.orange}}>1 failed</span>
          <span style={{color: C.dim}}>: parser::empty_input</span>
        </div>
        <div
          style={{
            marginTop: fontSize * 0.7,
            border: `2px solid ${C.hairStrong}`,
            borderRadius: 14,
            padding: '14px 20px',
            display: 'flex',
            gap: '0.6em',
            minHeight: fontSize * 1.5 * 2,
          }}
        >
          <span style={{color: C.dim}}>&gt;</span>
          <span style={{flex: 1}}>
            <DocText state={state} frame={frame} ink={C.paper} ghost="rgba(154,157,163,0.8)" caret={C.paper} />
          </span>
        </div>
        <div style={{color: C.ghost, fontSize: fontSize * 0.68, marginTop: 10, paddingLeft: 22}}>? for shortcuts</div>
      </div>
    </Win>
  );
};

const Caption: React.FC<{frame: number; size: number}> = ({frame, size}) => {
  const lines: [number, number, string][] = [
    [G1.showT, G2.showT, 'Tab accepts the whole sentence'],
    [G2.showT, SW, 'Tab again chains the next one'],
    [SW + SW_LEN, WITH_DUR + 1, 'Same keys in the terminal'],
  ];
  return (
    <div style={{position: 'relative', height: size * 1.3, flex: 1}}>
      {lines.map(([a, b, t]) => {
        const o = Math.min(
          interpolate(frame, [a, a + 6], [0, 1], clamp),
          interpolate(frame, [b - 6, b], [1, 0], clamp),
        );
        return (
          <div
            key={t}
            style={{
              position: 'absolute',
              inset: 0,
              textAlign: 'right',
              fontFamily: F.body,
              fontWeight: 500,
              fontSize: size,
              color: C.paper,
              opacity: o,
              transform: `translateY(${(1 - o) * 10}px)`,
              whiteSpace: 'nowrap',
            }}
          >
            {t}
          </div>
        );
      })}
    </div>
  );
};

export const With: React.FC<{layout: Layout}> = ({layout}) => {
  const frame = useCurrentFrame();
  const {fps} = useVideoConfig();
  const portrait = layout === 'portrait';
  const winW = portrait ? 960 : 1180;

  const enter = spring({frame, fps, config: {damping: 200}, durationInFrames: 20});
  const sw = interpolate(frame, [SW, SW + SW_LEN], [0, 1], {...clamp, easing: Easing.bezier(0.45, 0, 0.2, 1)});

  const email = docAt(EMAIL, frame);
  const term = docAt(TERM, frame);
  const fixes = email.fixes + term.fixes;
  const accepts = email.accepts + term.accepts;
  const secs = Math.round(activeFrames(frame) / 30);
  const done = frame >= DONE;
  const finale = pulseAt(frame, [DONE + 4], 24);

  const keyIn = spring({frame: frame - G1.showT, fps, config: {damping: 16, mass: 0.6}});
  const depth = pressDepth(frame, ACCEPTS);

  const statStyle: React.CSSProperties = portrait ? {width: 960} : {width: 444};
  const stats = (
    <div style={{display: 'flex', flexDirection: 'column', gap: portrait ? 20 : 24}}>
      <Stat
        label="Backspaces"
        value="0"
        color={C.greenText}
        sub={`${fixes} typo${fixes === 1 ? '' : 's'} fixed for you`}
        pulse={finale}
        style={statStyle}
      />
      <Stat
        label="Fix time"
        value="118 ms"
        color={C.paper}
        sub="per fix, on your GPU"
        pulse={Math.max(pulseAt(frame, FIXES, 9), finale)}
        style={statStyle}
      />
      <Stat
        label="Sentences"
        value={accepts}
        color={accepts > 0 ? C.blue : C.paper}
        sub={frame < T0 ? 'ready' : done ? `in ${secs} s` : `in ${secs} s, so far`}
        pulse={Math.max(pulseAt(frame, ACCEPTS, 10), finale)}
        style={statStyle}
      />
    </div>
  );

  const surfaceLabel = (text: string, o: number) => (
    <div
      style={{
        position: 'absolute',
        left: 0,
        top: 0,
        fontFamily: F.body,
        fontSize: 28,
        color: C.dim,
        opacity: o,
      }}
    >
      {text}
    </div>
  );

  const windows = (
    <div style={{position: 'relative', width: winW, height: portrait ? 664 : 612}}>
      {surfaceLabel('In your email', 1 - sw)}
      {surfaceLabel('In Claude Code, in the terminal', sw)}
      <div
        style={{
          position: 'absolute',
          left: 0,
          top: 52,
          opacity: 1 - sw,
          transform: `translateX(${-140 * sw}px) scale(${1 - 0.04 * sw})`,
        }}
      >
        <Email width={winW} fontSize={portrait ? 40 : 42} frame={frame} lines={portrait ? 6 : 4.9} />
      </div>
      {sw > 0 ? (
        <div style={{position: 'absolute', left: 0, top: 52, opacity: sw, transform: `translateX(${140 * (1 - sw)}px)`}}>
          <Terminal width={winW} fontSize={portrait ? 30 : 36} frame={frame} minHeight={portrait ? 540 : 490} />
        </div>
      ) : null}
    </div>
  );

  const keyRow = (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 36,
        width: winW,
        opacity: keyIn,
        transform: `translateY(${(1 - keyIn) * 30}px)`,
      }}
    >
      <Caption frame={frame} size={portrait ? 36 : 38} />
      <TabKey depth={depth} scale={portrait ? 1 : 1.05} />
    </div>
  );

  return (
    <AbsoluteFill style={{padding: portrait ? '110px 60px' : '60px 120px', justifyContent: 'center'}}>
      <Kicker text="With CaseComplete" color={C.blue} />
      <div
        style={{
          display: 'flex',
          flexDirection: portrait ? 'column' : 'row',
          gap: portrait ? 30 : 56,
          marginTop: portrait ? 36 : 40,
          opacity: enter,
          transform: `translateY(${(1 - enter) * 40}px)`,
        }}
      >
        <div style={{display: 'flex', flexDirection: 'column', gap: portrait ? 18 : 22}}>
          {windows}
          {keyRow}
        </div>
        {stats}
      </div>
    </AbsoluteFill>
  );
};
