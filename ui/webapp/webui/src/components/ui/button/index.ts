import { cva, type VariantProps } from 'class-variance-authority'

// shadcn-vue button variants re-skinned for the gold "instrument" identity.
export const buttonVariants = cva(
  [
    'inline-flex items-center justify-center gap-1.5 whitespace-nowrap rounded-md',
    'text-[13px] font-semibold transition-[color,background-color,box-shadow,transform]',
    'duration-150 ease-[var(--ease-out-soft)] select-none',
    'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary/45 focus-visible:ring-offset-1 focus-visible:ring-offset-bg',
    'disabled:pointer-events-none disabled:opacity-45',
    // An inert control must look inert: a disabled button that still lifts on
    // hover or scales on press advertises an interaction that will not happen.
    'disabled:transition-none disabled:transform-none disabled:shadow-none',
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
        // The committed destructive action: a solid deep fill with light ink,
        // rather than the tinted outline above. Used when the engine is up and
        // the only meaningful move is to stop it.
        'danger-solid':
          'border border-down-solid btn-down-solid',
        // The counterpart to a solid action: pale and flat, with no hover
        // affordance, for a control that is present but out of play. It keeps
        // full opacity because the base `disabled:opacity-45` would stack with
        // an already-pale fill and render it unreadable.
        idle: 'border border-line bg-panel-2 text-muted-fg disabled:opacity-100',
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
