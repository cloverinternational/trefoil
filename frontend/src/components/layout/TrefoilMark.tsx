import React from 'react';

interface TrefoilMarkProps {
  size?: number;
  className?: string;
}

let counter = 0;

// Trefoil: a three-leaf clover read as a connection. One line enters the top
// leaf, reaches a hub at the centre and splits down into the two lower leaves,
// the same one-endpoint-to-many-providers shape the gateway has.
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
        <path id={`${id}-l`} fill={`url(#${id}-g)`} d={LEAF} />
      </defs>
      <g transform="translate(32 36)">
        {[0, 120, 240].map(angle => (
          <use key={angle} href={`#${id}-l`} transform={`rotate(${angle}) translate(0 -1.5)`} />
        ))}
        <g stroke="#052E16" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
          <path d="M0 -23V0" />
          <path d="M0 0L15.6 9" />
          <path d="M0 0L-15.6 9" />
        </g>
        <path d="M0 -31V-23" stroke="#22C55E" strokeWidth="1.7" strokeLinecap="round" />
        <circle cy="-31" r="2" fill="#22C55E" />
        <g fill="#ECFDF5" stroke="#052E16" strokeWidth="1.1">
          <circle r="2.6" />
          <circle cx="15.6" cy="9" r="2.1" />
          <circle cx="-15.6" cy="9" r="2.1" />
        </g>
      </g>
    </svg>
  );
};
