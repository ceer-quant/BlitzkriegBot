import { cva, type VariantProps } from 'class-variance-authority'

// shadcn-vue button variants re-skinned for the gold "instrument" identity.
export const buttonVariants = cva(
  [
    'inline-flex items-center justify-center gap-1.5 whitespace-nowrap rounded-md',
    'text-[13px] font-semibold transition-[color,background-color,box-shadow,transform]',
    'duration-150 ease-[var(--ease-out-soft)] select-none',
    'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary/45 focus-visible:ring-offset-1 focus-visible:ring-offset-bg',
    'disabled:pointer-events-none disabled:opacity-45',
    'active:scale-[0.97]',
    '[&_svg]:size-4 [&_svg]:shrink-0',
  ].join(' '),
  {
    variants: {
      variant: {
        default:
          'bg-panel-2 text-fg border border-line hover:border-line-strong hover:bg-panel-solid/80',
        gold: 'btn-gold border border-primary/25',
        outline:
          'border border-line bg-transparent text-fg hover:bg-panel-2 hover:border-line-strong',
        ghost: 'text-muted-fg hover:text-fg hover:bg-panel-2',
        danger:
          'border border-down/30 bg-down/12 text-down hover:bg-down/20 hover:border-down/50',
        up: 'border border-up/30 bg-up/12 text-up hover:bg-up/20 hover:border-up/50',
      },
      size: {
        sm: 'h-8 px-3',
        default: 'h-9 px-4',
        lg: 'h-10 px-5 text-sm',
        icon: 'size-9',
        'icon-sm': 'size-7 rounded-[7px]',
      },
    },
    defaultVariants: { variant: 'default', size: 'default' },
  },
)

export type ButtonVariants = VariantProps<typeof buttonVariants>
