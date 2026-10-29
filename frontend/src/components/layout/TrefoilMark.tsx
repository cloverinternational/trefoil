import React from 'react';

interface TrefoilMarkProps {
  size?: number;
  className?: string;
}

let counter = 0;

// Trefoil: a three-leaf clover. Three heart-shaped leaves meet at the centre of
// a 64x64 box, shaded from a dark tip to a light edge, with a short curved stem.
const LEAF = "M0 0C-3 -3 -12 -9 -12 -17C-12 -22 -8.5 -25 -5 -25C-2.5 -25 -.8 -23.7 0 -22C.8 -23.7 2.5 -25 5 -25C8.5 -25 12 -22 12 -17C12 -9 3 -3 0 0Z";

export const TrefoilMark: React.FC<TrefoilMarkProps> = ({ size = 28, className }) => {
  const id = React.useMemo(() => `trefoil-mark-${++counter}`, []);
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 64 64"
      fill="none"
      aria-hidden="true"
      className={className}
    >
      <defs>
        <linearGradient id={`${id}-g`} x1="0" y1="1" x2="0" y2="0">
          <stop offset="0" stopColor="#15803D" />
          <stop offset=".55" stopColor="#22C55E" />
          <stop offset="1" stopColor="#86EFAC" />
        </linearGradient>
        <g id={`${id}-l`}>
          <path fill={`url(#${id}-g)`} d={LEAF} />
          <path d="M0 -4V-19" stroke="#052E16" strokeOpacity=".35" strokeWidth="1.2" strokeLinecap="round" />
        </g>
      </defs>
      <path d="M32 34C32 43 34 51 40 58" stroke="#15803D" strokeWidth="2.8" strokeLinecap="round" />
      <g transform="translate(32 33)">
        {[0, 120, 240].map(angle => (
          <use key={angle} href={`#${id}-l`} transform={`rotate(${angle}) translate(0 -1.5)`} />
        ))}
      </g>
    </svg>
  );
};
