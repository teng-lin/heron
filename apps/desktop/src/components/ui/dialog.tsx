/**
 * shadcn/ui-flavored Dialog.
 *
 * Wraps `@radix-ui/react-dialog` with project Tailwind classes. Used by
 * the Settings pane's Audio tab (purge confirmation) and About tab
 * (license modal) starting in PR-ζ (phase 68).
 *
 * Re-exports Radix's primitives so callers compose `<Dialog>` /
 * `<DialogTrigger>` / `<DialogContent>` etc. directly. The
 * `<DialogContent>` here owns the centered + ring + max-width layout
 * the rest of the app expects so call sites stay terse.
 */

import type * as React from "react";
import * as DialogPrimitive from "@radix-ui/react-dialog";
import { X } from "lucide-react";

import { cn } from "../../lib/cn";

export const Dialog = DialogPrimitive.Root;
export const DialogTrigger = DialogPrimitive.Trigger;
export const DialogPortal = DialogPrimitive.Portal;
export const DialogClose = DialogPrimitive.Close;

export interface DialogOverlayProps
  extends React.ComponentPropsWithoutRef<typeof DialogPrimitive.Overlay> {
  ref?: React.Ref<React.ElementRef<typeof DialogPrimitive.Overlay>>;
}

export function DialogOverlay({ className, ref, ...props }: DialogOverlayProps) {
  return (
    <DialogPrimitive.Overlay
      ref={ref}
      className={cn(
        "fixed inset-0 z-50 bg-black/50 backdrop-blur-sm",
        "data-[state=open]:animate-in data-[state=closed]:animate-out",
        "data-[state=closed]:fade-out-0 data-[state=open]:fade-in-0",
        className,
      )}
      {...props}
    />
  );
}

export interface DialogContentProps
  extends React.ComponentPropsWithoutRef<typeof DialogPrimitive.Content> {
  ref?: React.Ref<React.ElementRef<typeof DialogPrimitive.Content>>;
  /** Show the corner X close button. Defaults to `true`. */
  showCloseButton?: boolean;
}

export function DialogContent({
  className,
  children,
  ref,
  showCloseButton = true,
  ...props
}: DialogContentProps) {
  return (
    <DialogPortal>
      <DialogOverlay />
      <DialogPrimitive.Content
        ref={ref}
        className={cn(
          // The overflow-y-auto + max-h pair lets long license text
          // scroll inside the dialog instead of pushing the close
          // button off-screen.
          "fixed left-1/2 top-1/2 z-50 grid w-full max-w-2xl -translate-x-1/2 -translate-y-1/2",
          "max-h-[80vh] overflow-y-auto gap-4 border border-border bg-background p-6 shadow-lg",
          "rounded-lg",
          "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
          className,
        )}
        {...props}
      >
        {children}
        {showCloseButton && (
          <DialogPrimitive.Close
            aria-label="Close"
            className={cn(
              "absolute right-4 top-4 rounded-sm opacity-70 transition-opacity",
              "hover:opacity-100 focus-visible:outline-none focus-visible:ring-2",
              "focus-visible:ring-primary",
            )}
          >
            <X className="h-4 w-4" aria-hidden="true" />
          </DialogPrimitive.Close>
        )}
      </DialogPrimitive.Content>
    </DialogPortal>
  );
}

export function DialogHeader({
  className,
  ...props
}: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn("flex flex-col space-y-1.5 text-left", className)}
      {...props}
    />
  );
}

export function DialogFooter({
  className,
  ...props
}: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn(
        "flex flex-col-reverse gap-2 sm:flex-row sm:justify-end",
        className,
      )}
      {...props}
    />
  );
}

export interface DialogTitleProps
  extends React.ComponentPropsWithoutRef<typeof DialogPrimitive.Title> {
  ref?: React.Ref<React.ElementRef<typeof DialogPrimitive.Title>>;
}

export function DialogTitle({ className, ref, ...props }: DialogTitleProps) {
  return (
    <DialogPrimitive.Title
      ref={ref}
      className={cn("text-lg font-semibold leading-none", className)}
      {...props}
    />
  );
}

export interface DialogDescriptionProps
  extends React.ComponentPropsWithoutRef<typeof DialogPrimitive.Description> {
  ref?: React.Ref<React.ElementRef<typeof DialogPrimitive.Description>>;
}

export function DialogDescription({
  className,
  ref,
  ...props
}: DialogDescriptionProps) {
  return (
    <DialogPrimitive.Description
      ref={ref}
      className={cn("text-sm text-muted-foreground", className)}
      {...props}
    />
  );
}
