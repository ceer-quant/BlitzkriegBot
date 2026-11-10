import { defineConfig } from 'vitepress'
import { chineseSearchOptimize, pagefindPlugin } from 'vitepress-plugin-pagefind'
import { readdirSync, statSync } from 'node:fs'
import { join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'

const docsRoot = fileURLToPath(new URL('../', import.meta.url))

function collect(dir, acc = []) {
  for (const name of readdirSync(dir)) {
    if (name.startsWith('.') || name === 'index.md') continue
    const p = join(dir, name)
    if (statSync(p).isDirectory()) collect(p, acc)
    else if (name.endsWith('.md')) acc.push(relative(docsRoot, p))
  }
  return acc
}

const groups = new Map()
for (const rel of collect(docsRoot).sort()) {
  const dir = rel.includes('/') ? rel.split('/')[0] : '根目录'
  if (!groups.has(dir)) groups.set(dir, [])
  groups.get(dir).push({
    text: rel.split('/').pop().replace(/\.md$/, ''),
    link: '/' + rel.replace(/\.md$/, ''),
  })
}

// 临时：现有文档存在 14 处死链与 23 篇裸占位符，先容忍构建，
// 待 #1080 文档重写完成后改为 false，届时死链必须清零。

// GitHub Pages 项目站点发布在 /BlitzkriegBot/ 下，由工作流注入；本地开发保持根路径。
// head 里的资源路径不会被 VitePress 自动加 base，必须自己拼，否则 favicon 会指向根域名而 404。
const base = process.env.DOCS_BASE || '/'

export default defineConfig({
  base,
  title: 'BlitzkriegBot',
  description: 'BlitzkriegBot 文档：入门、概念、任务、参考、排障、决策',
  lang: 'zh-CN',
  cleanUrls: true,
  lastUpdated: true,
  ignoreDeadLinks: true,
  // 亮/暗双模式；首次访问默认深色（与 WebUI 一致），之后记住用户选择
  appearance: true,
  head: [
    ['meta', { name: 'theme-color', content: '#11141b' }],
    ['link', { rel: 'icon', type: 'image/png', href: `${base}favicon.png` }],
    [
      'script',
      {},
      "(function(){try{var k='vitepress-theme-appearance',v=localStorage.getItem(k);if(!v){localStorage.setItem(k,'dark');document.documentElement.classList.add('dark')}}catch(e){}})()",
    ],
    [
      'script',
      {},
      "(function(){var d=document,el=null,light='#f6f6f8',dark='#11141b';function sync(){el=el||d.querySelector('meta[name=theme-color]');if(!el)return;el.setAttribute('content',d.documentElement.classList.contains('dark')?dark:light)}d.addEventListener('DOMContentLoaded',sync);new MutationObserver(sync).observe(d.documentElement,{attributes:true,attributeFilter:['class']})})()",
    ],
  ],
  vite: {
    plugins: [
      pagefindPlugin({
        forceLanguage: 'zh-cn',
        customSearchQuery: chineseSearchOptimize,
        btnPlaceholder: '搜索',
        placeholder: '搜索文档（支持中文）',
        emptyText: '没有找到相关内容',
        heading: '共 {{searchResult}} 条结果',
      }),
    ],
  },
  themeConfig: {
    logo: '/logo.png',
    siteTitle: 'BlitzkriegBot',
    outline: { level: [2, 3], label: '本页目录' },
    nav: [
      { text: '开始', link: '/getting-started' },
      { text: '文档', link: '/FEATURES' },
      { text: '术语表', link: '/glossary' },
    ],
    docFooter: { prev: '上一篇', next: '下一篇' },
    sidebar: [
      {
        text: '开始',
        items: [
          { text: '文档首页', link: '/' },
          { text: '术语表', link: '/glossary' },
        ],
      },
      ...Array.from(groups, ([dir, items]) => ({ text: dir, collapsed: true, items })),
    ],
    socialLinks: [{ icon: 'github', link: 'https://github.com/ceer-quant/BlitzkriegBot' }],
    footer: {
      message:
        '🎵Song Engine | 🪨 Stone Backtester | 🦄 PonyYiDan UI Kit<br />Build Once, Trade Everywhere. 让个人拥有机构级的交易能力。',
      copyright: 'BlitzkriegBot，Built with ❤️ by fancer',
    },
  },
})
