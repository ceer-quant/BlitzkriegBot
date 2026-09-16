<script setup lang="ts">
import { TooltipProvider, TooltipRoot, TooltipTrigger, TooltipPortal, TooltipContent } from 'reka-ui'
import { cn } from '@/lib/utils'

const props = withDefaults(defineProps<{ content: string; side?: 'top' | 'right' | 'bottom' | 'left'; class?: string }>(), {
  side: 'top',
})
</script>

<template>
  <TooltipProvider :delay-duration="260">
    <TooltipRoot>
      <TooltipTrigger as-child>
        <slot />
      </TooltipTrigger>
      <TooltipPortal>
        <TooltipContent
          :side="props.side"
          :side-offset="6"
          :class="cn(
            'z-50 max-w-[280px] rounded-md border border-line-strong bg-panel-solid px-2.5 py-1.5',
            'text-[11.5px] font-medium text-fg shadow-[var(--shadow-2)]',
            'data-[state=delayed-open]:animate-[rise-in_.16s_var(--ease-out-soft)]',
          )"
        >
          {{ props.content }}
        </TooltipContent>
      </TooltipPortal>
    </TooltipRoot>
  </TooltipProvider>
</template>
