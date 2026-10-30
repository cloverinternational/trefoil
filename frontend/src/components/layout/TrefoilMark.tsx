import React from 'react';

interface TrefoilMarkProps {
  size?: number;
  className?: string;
}

// The Trefoil mark: three equal lobes, each bitten by its clockwise neighbour,
// whose channels meet at one central point. Paths come from docs/brand/build.py
// (docs/assets/brand/paths.json); the small cut is used at 24px and below.
const MARK = "M45.39 18.25C45.39 19.67 45.17 21.05 44.75 22.33C44.54 22.33 44.32 22.32 44.1 22.32C37.42 22.32 31.57 25.83 28.28 31.11C22.69 29.5 18.61 24.35 18.61 18.25C18.61 10.85 24.61 4.86 32 4.86C39.39 4.86 45.39 10.85 45.39 18.25ZM31.78 41.82C31.78 49.22 25.78 55.21 18.39 55.21C10.99 55.21 5 49.22 5 41.82C5 35.4 9.52 30.04 15.55 28.74C18.59 34.45 24.52 38.39 31.39 38.61C31.64 39.64 31.78 40.72 31.78 41.82ZM59 41.82C59 49.22 53.01 55.21 45.61 55.21C41.68 55.21 38.15 53.52 35.7 50.82C37.49 47.96 38.53 44.58 38.53 40.95C38.53 37.78 37.73 34.79 36.33 32.17C38.74 29.86 42.01 28.43 45.61 28.43C53.01 28.43 59 34.43 59 41.82Z";
const SMALL = "M46.44 16.28C46.44 17.91 46.17 19.49 45.66 20.96C38.66 21.29 32.52 24.93 28.77 30.35C22.35 28.89 17.56 23.14 17.56 16.28C17.56 8.3 24.03 1.84 32 1.84C39.97 1.84 46.44 8.3 46.44 16.28ZM30.88 43.23C30.88 51.2 24.41 57.67 16.44 57.67C8.46 57.67 2 51.2 2 43.23C2 36.21 7.01 30.36 13.66 29.06C17.24 34.61 23.28 38.44 30.25 38.99C30.66 40.33 30.88 41.75 30.88 43.23ZM62 43.23C62 51.2 55.54 57.67 47.56 57.67C43.22 57.67 39.33 55.75 36.68 52.72C38.22 49.73 39.09 46.34 39.09 42.75C39.09 39.4 38.34 36.23 36.99 33.39C39.63 30.56 43.39 28.79 47.56 28.79C55.54 28.79 62 35.26 62 43.23Z";

export const TrefoilMark: React.FC<TrefoilMarkProps> = ({ size = 28, className }) => (
  <svg width={size} height={size} viewBox="0 0 64 64" aria-hidden="true" className={className}>
    <path fill="var(--brand-mark)" d={size <= 24 ? SMALL : MARK} />
  </svg>
);
