---
layout: page
sidebar: false
aside: false
pageClass: bb-landing
title: BlitzkriegBot
description: 独立交易者的 TradingOS —— Market-Agnostic Core、Risk-First Engine、Order Flow Engine。
---

<section class="bb-hero">
  <div class="bb-hero-copy">
    <p class="bb-eyebrow">TradingOS for independent traders</p>
    <h1 class="bb-hero-title">BlitzkriegBot</h1>
    <p class="bb-hero-tag">独立交易者的 TradingOS</p>
    <p class="bb-hero-slogan">Build Once, Trade Everywhere.</p>
    <p class="bb-hero-pitch">不是又一个量化框架。Market-Agnostic Core、Risk-First Engine 与 Order Flow Engine 拼在一起，让个人交易者拥有机构级的交易基础设施。</p>
    <div class="bb-actions">
      <a class="bb-btn bb-btn-brand" href="./getting-started">立即开始</a>
      <a class="bb-btn" href="#architecture">查看架构</a>
      <a class="bb-btn" href="https://github.com/ceer-quant/BlitzkriegBot" target="_blank" rel="noreferrer">GitHub</a>
    </div>
  </div>
  <div class="bb-hero-shot">
    <figure class="bb-shot">
      <img src="./screenshots/panel-blueprint.png" alt="BlitzkriegBot 控制面板：Blueprint 运行视图" loading="lazy" />
      <figcaption>真实运行画面 · Blueprint 面板</figcaption>
    </figure>
  </div>
</section>

<section class="bb-sec bb-brands">
  <p class="bb-eyebrow">The stack</p>
  <h2 class="bb-h2">三件法宝</h2>
  <div class="bb-cards">
    <article class="bb-card bb-brand-card">
      <span class="bb-brand-mark is-song" aria-hidden="true"></span>
      <h3 class="bb-brand-name">Song</h3>
      <p class="bb-brand-role">超强性能 · 订单管理</p>
      <p class="bb-brand-desc">Rust 原生，没有 GC、没有虚拟机。参数热路径用 ArcSwap 锁自由读，结构变更只发生在装配时；挂单、改价、撤单、成交与记账走同一条同步路径。</p>
    </article>
    <article class="bb-card bb-brand-card">
      <span class="bb-brand-mark is-stone" aria-hidden="true"></span>
      <h3 class="bb-brand-name">Stone</h3>
      <p class="bb-brand-role">与实盘完全一致 · 回测</p>
      <p class="bb-brand-desc">回测跑的就是实盘那个内核：同一份策略包、同一套撮合与费用、同一本账，只把交易所换成历史盘口重放。回测与实盘，不会各说各话。</p>
    </article>
    <article class="bb-card bb-brand-card">
      <span class="bb-brand-mark is-ui" aria-hidden="true"></span>
      <h3 class="bb-brand-name">PonyYiDan</h3>
      <p class="bb-brand-role">现代好看 · 开箱易用</p>
      <p class="bb-brand-desc">Vue 3 + TypeScript 的 WebUI：玻璃质感、亮暗双主题，鼠标键盘都顺手；服务器上没有图形界面时，纯 Rust 的 TUI 读同一份内核快照。</p>
    </article>
  </div>
  <p class="bb-note">三个名字，一条内核 —— 不是三套系统拼起来的外壳。</p>
</section>

<section class="bb-sec">
  <p class="bb-eyebrow">Why different</p>
  <h2 class="bb-h2">为什么 BlitzkriegBot 不一样？</h2>
  <div class="bb-cards">
    <article class="bb-card">
      <h3>Market-Agnostic Core</h3>
      <p class="bb-card-lead">核心完全不知道市场。</p>
      <ul class="bb-list">
        <li><span class="bb-badge on">已接入</span> Polymarket</li>
        <li><span class="bb-badge on">已接入</span> Kalshi</li>
        <li><span class="bb-badge on">已接入</span> PredictFun</li>
        <li><span class="bb-badge on">已接入</span> PolySimulator（仿真）</li>
        <li><span class="bb-badge off">Roadmap</span> Crypto Spot / Futures / Options / Stocks</li>
      </ul>
      <p class="bb-card-foot">市场在变，交易内核不变。新市场 = 新插件。</p>
    </article>
    <article class="bb-card">
      <h3>Risk First</h3>
      <p class="bb-card-lead">策略不能绕过风控。</p>
      <ul class="bb-list">
        <li>每日亏损熔断（Loss Breaker）</li>
        <li>Readonly / Dry Run 三态</li>
        <li>持仓上限与组合级闸门</li>
        <li>单账户冻结与全局 kill</li>
      </ul>
      <p class="bb-card-foot">先活下来，再赚钱。</p>
    </article>
    <article class="bb-card">
      <h3>Order Flow Native</h3>
      <p class="bb-card-lead">盯着盘口做交易，不是盯着 K 线。</p>
      <ul class="bb-list">
        <li>挂单 / 撤单 / 改价全链路</li>
        <li>盘口深度与队列位置</li>
        <li>成交流与成交概率</li>
        <li>风控拒绝也能看见原因</li>
      </ul>
      <p class="bb-card-foot">Alpha 来自执行质量。</p>
    </article>
  </div>
</section>

<section class="bb-sec" id="architecture">
  <p class="bb-eyebrow">Architecture</p>
  <h2 class="bb-h2">TradingOS Architecture</h2>
  <div class="bb-arch">
    <div class="bb-arch-box"><strong>Strategy Layer</strong><span>Blueprint / Lua</span></div>
    <div class="bb-arrow">↓</div>
    <div class="bb-arch-box"><strong>Risk Engine</strong><span>风控闸门 · 先于下单</span></div>
    <div class="bb-arrow">↓</div>
    <div class="bb-arch-box accent"><strong>Core Engine</strong><span>Rust · 订单与账本唯一真相</span></div>
    <div class="bb-arrow">↓</div>
    <div class="bb-arch-box"><strong>Market API</strong><span>统一市场接口</span></div>
    <div class="bb-arrow">↓</div>
    <div class="bb-markets">
      <span class="bb-market on">Polymarket</span>
      <span class="bb-market on">Kalshi</span>
      <span class="bb-market on">PredictFun</span>
      <span class="bb-market on">PolySimulator</span>
      <span class="bb-market off">Crypto</span>
      <span class="bb-market off">Futures</span>
      <span class="bb-market off">Stocks</span>
    </div>
  </div>
  <p class="bb-note">核心交易引擎永远不依赖具体市场。新市场 = 新插件。</p>
</section>

<section class="bb-sec" id="lifecycle">
  <p class="bb-eyebrow">Lifecycle</p>
  <h2 class="bb-h2">从一个想法到实盘</h2>
  <div class="bb-flow">
    <span class="bb-step">Idea</span>
    <span class="bb-flow-arrow">→</span>
    <span class="bb-step">Blueprint</span>
    <span class="bb-flow-arrow">→</span>
    <span class="bb-step">Backtest</span>
    <span class="bb-flow-arrow">→</span>
    <span class="bb-step">Simulation</span>
    <span class="bb-flow-arrow">→</span>
    <span class="bb-step">Risk Validation</span>
    <span class="bb-flow-arrow">→</span>
    <span class="bb-step accent">Live</span>
  </div>
  <p class="bb-note">同一套策略包贯穿全流程 —— 不是回测一套代码、实盘另一套代码。</p>
</section>

<section class="bb-sec">
  <p class="bb-eyebrow">Order flow</p>
  <h2 class="bb-h2">Order Flow Native</h2>
  <p class="bb-lede">闪电战不是 K 线交易系统，而是订单流交易系统。策略关心的是盘口、深度、队列、流动性与成交概率，而不是指标预测。</p>
  <div class="bb-cards bb-cards-2">
    <article class="bb-card">
      <h3>看得见的执行</h3>
      <ul class="bb-list">
        <li>每一笔挂单都有价格、数量与档位</li>
        <li>撤单与改价不再黑箱</li>
        <li>maker / taker 分开记账</li>
      </ul>
    </article>
    <article class="bb-card">
      <h3>看得见的拒绝</h3>
      <ul class="bb-list">
        <li>被风控拦下的单带着原因与时间</li>
        <li>账户冻结、场地拒单、配置态统一进同一个闸门</li>
        <li>面板与实盘读同一份账本</li>
      </ul>
    </article>
  </div>
</section>

<section class="bb-sec">
  <p class="bb-eyebrow">Real screenshots</p>
  <h2 class="bb-h2">真实运行画面</h2>
  <div class="bb-shots">
    <figure class="bb-shot">
      <img src="./screenshots/panel-blueprint.png" alt="Blueprint 面板" loading="lazy" />
      <figcaption>控制面板 · Blueprint</figcaption>
    </figure>
    <figure class="bb-shot">
      <img src="./screenshots/panel-markets.png" alt="事件市场面板" loading="lazy" />
      <figcaption>事件市场 · BTC UP/DOWN</figcaption>
    </figure>
    <figure class="bb-shot">
      <img src="./screenshots/panel-strategies.png" alt="策略管理面板" loading="lazy" />
      <figcaption>策略管理 · Lua</figcaption>
    </figure>
  </div>
  <p class="bb-note">全部为实际运行截图，不是渲染图。</p>
</section>

<section class="bb-sec">
  <p class="bb-eyebrow">Numbers</p>
  <h2 class="bb-h2">Built Fast. Built Real.</h2>
  <div class="bb-stats">
    <RepoStat kind="changes" label="Commits" />
    <RepoStat kind="pullRequests" label="Pull Requests" />
    <div class="bb-stat"><b data-count="100" data-suffix="%">100%</b><span>Rust Core</span></div>
    <div class="bb-stat"><b data-count="4">4</b><span>已接入市场</span></div>
  </div>
  <p class="bb-note">Commits 与 Pull Requests 是上一稳定版到最新 Release 的数量，和顶栏公告同一份数据。读不到发布记录时这两格不显示。Dry → Simulation → Live，同一条链路。</p>
</section>

<section class="bb-sec" id="roadmap">
  <p class="bb-eyebrow">Roadmap</p>
  <h2 class="bb-h2">走到哪了</h2>
  <div class="bb-cards bb-cards-2">
    <article class="bb-card">
      <h3>已交付</h3>
      <ul class="bb-list check">
        <li>Core Engine</li>
        <li>Risk Engine</li>
        <li>Blueprint</li>
        <li>Polymarket / Kalshi / PredictFun 接入</li>
        <li>PolySimulator（dry 与回测）</li>
        <li>WebUI + TUI</li>
      </ul>
    </article>
    <article class="bb-card">
      <h3>下一步</h3>
      <ul class="bb-list todo">
        <li>Binance Spot</li>
        <li>Futures</li>
        <li>Stocks</li>
        <li>Market Making Engine</li>
        <li>Alpha Marketplace</li>
      </ul>
    </article>
  </div>
</section>

<section class="bb-sec">
  <p class="bb-eyebrow">For developers</p>
  <h2 class="bb-h2">For Developers</h2>
  <pre class="bb-code"><code>cargo build --release
(cd ui/webapp/webui && npm ci && npm run build)
blitzkrieg run</code></pre>
  <div class="bb-tags">
    <span>Blueprint</span><span>Lua</span><span>Market API</span><span>Plugin System</span><span>WebUI</span><span>TUI</span>
  </div>
  <p class="bb-note">Everything Extensible.</p>
</section>

<section class="bb-sec bb-mission">
  <p class="bb-eyebrow">Mission</p>
  <h2 class="bb-h2">为什么叫 BlitzkriegBot？</h2>
  <p>因为我们信奉效率与可靠。闪电战讲的是决策快、执行快，并且在压力下不掉链子 —— 这套标准，我们原样搬进了交易系统。</p>
  <p>今天，机构拥有做市系统、风控系统、仿真系统，而个人交易者手里通常只有指标和 K 线。</p>
  <p>BlitzkriegBot 想改变这件事：用 Rust 写一套订单簿级的高性能交易框架，让个人交易者拿到与机构同一档的执行质量。</p>
  <p class="bb-quote">我们的目标不是制造更多交易机器人，而是为独立交易者提供属于自己的 TradingOS。</p>
</section>
