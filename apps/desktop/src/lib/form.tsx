// Pre-bound TanStack Form hook (useAppForm) + reusable field/form components, per
// the project convention. Fields wrap shadcn primitives and bind to field context.
import { createFormHook, createFormHookContexts } from "@tanstack/react-form";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";

const { fieldContext, formContext, useFieldContext, useFormContext } = createFormHookContexts();

function TextField({
  type = "text",
  placeholder,
  className,
  disabled,
}: {
  type?: string;
  placeholder?: string;
  className?: string;
  disabled?: boolean;
}) {
  const field = useFieldContext<string>();
  return (
    <Input
      type={type}
      className={className}
      placeholder={placeholder}
      disabled={disabled}
      value={field.state.value}
      onChange={(e) => field.handleChange(e.currentTarget.value)}
      onBlur={field.handleBlur}
    />
  );
}

function CheckboxField({ className }: { className?: string }) {
  const field = useFieldContext<boolean>();
  return (
    <Checkbox
      className={className}
      checked={field.state.value}
      onCheckedChange={(checked) => field.handleChange(checked === true)}
    />
  );
}

function SubmitButton({ children }: { children: React.ReactNode }) {
  const form = useFormContext();
  return (
    <form.Subscribe selector={(s) => ({ canSubmit: s.canSubmit, isSubmitting: s.isSubmitting })}>
      {({ canSubmit, isSubmitting }) => (
        <Button type="submit" size="sm" disabled={!canSubmit || isSubmitting}>
          {children}
        </Button>
      )}
    </form.Subscribe>
  );
}

export const { useAppForm } = createFormHook({
  fieldContext,
  formContext,
  fieldComponents: { TextField, CheckboxField },
  formComponents: { SubmitButton },
});
