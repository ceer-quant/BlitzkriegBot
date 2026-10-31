/**
 * 盘口/K线的选中状态（issue 781：从 HftPage 的脚本搬出的 domain 逻辑）。
 *
 * 划分：页面持有原始行读数（prices / books / hasDepthData）与 venue 跟随门
 * （visiblePrices / visibleBooks 是钉在页面里的 Loading + venue 过滤出口，
 * 门禁按逐字源码形状钉住）；这个 composable 只拥有「选中哪个资产、哪一侧
 * token、K线是否可画」。它的输入是过滤后的 visibleBooks，所以切户清空旧所
 * 行时，选中项会随可见行重绑到新所首行。
 */
import { computed, ref, watch, type ComputedRef } from 'vue'
import type { AssetBook, Snapshot } from '@/api/client'

export function useTradeBook(
  snap: ComputedRef<Snapshot | null>,
  visibleBooks: ComputedRef<AssetBook[]>,
) {
  const depthAsset = ref('')
  // 资产选择跟随 venue 过滤后的可见行：旧所的资产不会成为新所页面的选中项
  // （切换账户清空旧所行时，选中资产随 visibleBooks 重置到新所首行）。
  watch(visibleBooks, (b) => {
    if (!b.length) {
      depthAsset.value = ''
      return
    }
    if (!b.some((x) => x.asset === depthAsset.value)) depthAsset.value = b[0].asset
  }, { immediate: true })

  /** UP or DOWN token of the selected asset — the two halves of the round. */
  const depthToken = ref<'up' | 'down'>('up')

  const activeBook = computed(() => visibleBooks.value.find((b) => b.asset === depthAsset.value) ?? null)
  // ── K线 (E29, kline.history) ───────────────────────────────────────────────
  /**
   * The aggregator keys prediction-token bars by TOKEN id (what `feed_klines`
   * was fed), not by asset name — the same key the books view now exposes.
   * Empty on older cores: the card hides rather than queries a wrong key.
   *
   * K线只属于 CEX 现货/合约：二元预测市场（prediction）的价格就是 0..1 概率，
   * 没有 OHLC 概念，卡片必须藏——哪怕行情插件带了真 token id。
   */
  const KLINE_MARKET_TYPES = new Set(['spot', 'futures', 'options'])
  const klineReady = computed(() => {
    if (!KLINE_MARKET_TYPES.has(snap.value?.marketActiveType ?? '')) return false
    const b = activeBook.value
    if (!b) return false
    return depthToken.value === 'up' ? !!b.upTokenId : !!b.downTokenId
  })
  const klineSymbol = computed(() => {
    const b = activeBook.value
    if (!b) return ''
    return depthToken.value === 'up' ? b.upTokenId : b.downTokenId
  })

  return { depthAsset, depthToken, activeBook, klineReady, klineSymbol }
}
