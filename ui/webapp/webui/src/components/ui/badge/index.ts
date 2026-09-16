import { cva, type VariantProps } from 'class-variance-authority'

export const badgeVariants = cva(
  'inline-flex items-center gap-1 rounded-full border px-2.5 py-[3px] text-[11px] font-semibold leading-none whitespace-nowrap',
  {
    variants: {
      variant: {
        default: 'border-line bg-panel-2 text-muted-fg',
        gold: 'border-primary/30 bg-primary/14 text-primary',
        up: 'border-up/30 bg-up/12 text-up',
        down: 'border-down/30 bg-down/12 text-down',
        info: 'border-info/30 bg-info/12 text-info',
        outline: 'border-line-strong bg-transparent text-fg',
      },
    },
    defaultVariants: { variant: 'default' },
  },
)

export type BadgeVariants = VariantProps<typeof badgeVariants>
