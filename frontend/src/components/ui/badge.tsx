import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";
import { CircleAlert, CircleCheck } from "lucide-react";
import { cn } from "../../lib/utils";

const badgeVariants = cva(
  "inline-flex items-center gap-1 border px-1.5 py-0 text-[11px] uppercase tracking-wide font-medium transition-colors",
  {
    variants: {
      variant: {
        default: "border-transparent bg-primary text-primary-foreground",
        secondary: "border-transparent bg-secondary text-secondary-foreground",
        destructive: "border-transparent bg-destructive text-destructive-foreground",
        success: "border-transparent bg-success/15 text-success",
        outline: "text-foreground",
      },
    },
    defaultVariants: { variant: "default" },
  },
);

export interface BadgeProps
  extends React.HTMLAttributes<HTMLDivElement>,
    VariantProps<typeof badgeVariants> {}

// Success and destructive carry an icon so their meaning never rests on color alone.
const STATUS_ICON = { success: CircleCheck, destructive: CircleAlert } as const;

function Badge({ className, variant, children, ...props }: BadgeProps) {
  const Icon = variant === "success" || variant === "destructive" ? STATUS_ICON[variant] : null;
  return (
    <div className={cn(badgeVariants({ variant }), className)} {...props}>
      {Icon && <Icon aria-hidden="true" className="h-3 w-3 shrink-0" />}
      {children}
    </div>
  );
}

export { Badge, badgeVariants };
