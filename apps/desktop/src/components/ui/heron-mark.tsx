/**
 * Heron mark — thin geometric S-curve neck profile, hand-tuned by the
 * design prototype at `.design/atoms.jsx:4`. Hand-tuned: changing the
 * `circle` head position or the cubic on the neck path will look off.
 */

import type * as React from "react";

export interface HeronMarkProps extends React.SVGAttributes<SVGSVGElement> {
  size?: number;
  color?: string;
}

export function HeronMark({
  size = 18,
  color = "currentColor",
  className,
  ...props
}: HeronMarkProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke={color}
      strokeWidth="1.4"
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      style={{ display: "inline-block", verticalAlign: "middle" }}
      {...props}
    >
      <circle cx="17" cy="5.5" r="1.1" fill={color} stroke="none" />
      <path d="M18.1 5.5 L21.5 5.1" />
      <path d="M16.5 6.3 C 15 8, 16.5 10, 14.5 11.5 C 12 13.5, 13 16, 12 18" />
      <path d="M9 18 C 10.5 18.4, 13.5 18.4, 15.5 17.6 C 16.5 17.2, 17 16.5, 16.5 15.5" />
      <path d="M8.5 18 L 6 16.8" />
      <path d="M11 18 L 10.5 22" />
      <path d="M13 18 L 13.5 22" />
    </svg>
  );
}
