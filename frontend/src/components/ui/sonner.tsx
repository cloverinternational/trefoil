import { Toaster as Sonner } from "sonner";

const Toaster = () => (
  <Sonner
    theme="dark"
    position="bottom-right"
    toastOptions={{
      style: {
        background: "var(--card)",
        border: "1px solid var(--border)",
        color: "var(--foreground)",
      },
    }}
  />
);

export { Toaster };
