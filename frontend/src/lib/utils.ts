import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/** Formats a usage-series bucket timestamp for chart axes. */
export function bucketLabel(ms: number, range: string): string {
  const options: Intl.DateTimeFormatOptions = range === 'day'
    ? { weekday: 'short', hour: '2-digit', minute: '2-digit' }
    : { month: 'short', day: 'numeric' };
  return new Date(ms).toLocaleString(undefined, options);
}
