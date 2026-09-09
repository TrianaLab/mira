<script>
  import { fmtTime, COLORS } from './api.js'

  let { series } = $props()
  let canvas = $state(null)
  // Exemplar screen positions from the last draw, so a click can find the trace
  // under the cursor. Rebuilt every draw, which is also every resize.
  let marks = []

  // A time-series line chart in about a hundred lines of canvas.
  //
  // Deliberately not a charting library. uPlot is 22 KB gzipped and the 5% of it
  // this screen uses is the part written below; Chart.js is four times that.
  // Svelte gives us the component boundary and the reactivity, which is the part
  // worth importing -- the pixels are ours.
  function draw() {
    if (!canvas || !series?.length) return
    const dpr = window.devicePixelRatio || 1
    const w = canvas.clientWidth
    const h = canvas.clientHeight
    if (!w || !h) return
    canvas.width = w * dpr
    canvas.height = h * dpr
    const c = canvas.getContext('2d')
    c.setTransform(dpr, 0, 0, dpr, 0, 0)
    c.clearRect(0, 0, w, h)

    const css = getComputedStyle(document.documentElement)
    const dim = css.getPropertyValue('--dim').trim()
    const line = css.getPropertyValue('--line').trim()

    const pad = { l: 62, r: 10, t: 10, b: 22 }
    const pw = w - pad.l - pad.r
    const ph = h - pad.t - pad.b
    if (pw <= 0 || ph <= 0) return

    let x0 = Infinity, x1 = -Infinity, y0 = Infinity, y1 = -Infinity
    const extend = (x, y) => {
      if (x < x0) x0 = x
      if (x > x1) x1 = x
      if (y === null || y === undefined) return
      if (y < y0) y0 = y
      if (y > y1) y1 = y
    }
    for (const s of series) {
      for (const [x, y] of s.points) extend(x, y)
      // Exemplars are in the extent too. An exemplar is a single measurement,
      // not an aggregate, so it routinely sits outside the range of the line it
      // belongs to — and one drawn off the plot is one nobody can click.
      for (const e of s.exemplars || []) extend(Number(e.time_unix_nano), value(e))
    }
    if (!Number.isFinite(x0) || !Number.isFinite(y0)) return
    // A flat series deserves a line through the middle rather than a division by
    // zero, and a chart whose baseline is not zero lies about ratios, so a
    // series that stays in the upper half keeps its zero.
    if (y0 === y1) { y0 -= 1; y1 += 1 }
    if (y0 > 0 && y0 < y1 / 2) y0 = 0

    const sx = (x) => pad.l + (x1 === x0 ? pw / 2 : ((x - x0) / (x1 - x0)) * pw)
    const sy = (y) => pad.t + ph - ((y - y0) / (y1 - y0)) * ph

    c.font = '11px ui-monospace, monospace'
    c.strokeStyle = line
    c.fillStyle = dim
    c.lineWidth = 1
    for (let i = 0; i <= 4; i++) {
      const y = pad.t + (ph * i) / 4
      c.beginPath()
      // Half-pixel offset: a 1px line on an integer coordinate straddles two
      // device pixels and renders as a 2px smear.
      c.moveTo(pad.l, Math.round(y) + 0.5)
      c.lineTo(w - pad.r, Math.round(y) + 0.5)
      c.stroke()
      c.textAlign = 'right'
      c.fillText(tick(y1 - ((y1 - y0) * i) / 4), pad.l - 8, y + 4)
    }
    c.textAlign = 'center'
    for (let i = 0; i <= 3; i++) {
      const x = x0 + ((x1 - x0) * i) / 3
      c.fillText(fmtTime(x).slice(6), sx(x), h - 6)
    }

    c.lineWidth = 1.5
    c.lineJoin = 'round'
    // Points with no neighbour to join. A subpath of one moveTo strokes nothing
    // at all, so a series of a single point -- which is every series in the
    // window after one SDK export interval -- would paint a blank chart. A gap
    // in the middle of a dense one has the same problem, and this catches both.
    // Collected rather than filled here: the line's path is still open.
    const dots = []
    series.forEach((s, i) => {
      c.strokeStyle = COLORS[i % COLORS.length]
      c.beginPath()
      let pen = false
      s.points.forEach(([x, y], k) => {
        // A null is a non-finite value the engine refused to fake -- a histogram
        // sum over no observations is NaN. Break the line rather than
        // interpolating across a gap that has no data in it.
        if (y === null) { pen = false; return }
        const px = sx(x)
        const py = sy(y)
        if (pen) c.lineTo(px, py)
        else {
          c.moveTo(px, py)
          if ((s.points[k + 1]?.[1] ?? null) === null) dots.push([px, py, i])
        }
        pen = true
      })
      c.stroke()
    })
    for (const [px, py, i] of dots) {
      c.fillStyle = COLORS[i % COLORS.length]
      c.beginPath()
      c.arc(px, py, 2.5, 0, Math.PI * 2)
      c.fill()
    }

    // The metric-to-trace edge, on screen. A diamond per exemplar; clicking one
    // opens the trace that produced that measurement.
    marks = []
    series.forEach((s, i) => {
      c.fillStyle = COLORS[i % COLORS.length]
      c.strokeStyle = css.getPropertyValue('--bg').trim() || '#000'
      for (const e of s.exemplars || []) {
        const v = value(e)
        if (v === null || !e.trace_id) continue
        const px = sx(Number(e.time_unix_nano))
        const py = sy(v)
        c.beginPath()
        c.moveTo(px, py - 4)
        c.lineTo(px + 4, py)
        c.lineTo(px, py + 4)
        c.lineTo(px - 4, py)
        c.closePath()
        c.fill()
        c.stroke()
        marks.push({ px, py, trace: e.trace_id })
      }
    })
  }

  // An exemplar's value: `int` and `double` are separate columns because an
  // OTLP counter past 2^53 loses its low bits as a float.
  const value = (e) => (e.double ?? e.int ?? null)

  function near(ev) {
    const r = canvas.getBoundingClientRect()
    const x = ev.clientX - r.left
    const y = ev.clientY - r.top
    let best = null
    for (const m of marks) {
      const d = Math.hypot(m.px - x, m.py - y)
      if (d <= 8 && (!best || d < best.d)) best = { d, trace: m.trace }
    }
    return best
  }

  function tick(v) {
    const a = Math.abs(v)
    if (a >= 1e9) return (v / 1e9).toFixed(1) + 'G'
    if (a >= 1e6) return (v / 1e6).toFixed(1) + 'M'
    if (a >= 1e3) return (v / 1e3).toFixed(1) + 'k'
    if (a === 0 || a >= 1) return String(Math.round(v * 100) / 100)
    return v.toPrecision(2)
  }

  $effect(() => {
    void series
    draw()
    // Redraw on resize rather than letting the browser scale a stretched
    // bitmap; a chart that goes blurry when the window moves reads as broken.
    const ro = new ResizeObserver(draw)
    ro.observe(canvas)
    return () => ro.disconnect()
  })
</script>

<!-- Hit testing rather than a DOM node per exemplar: the markers are painted
     pixels, so the only thing the element can offer is the cursor position. -->
<canvas
  bind:this={canvas}
  onclick={(e) => { const h = near(e); if (h) location.hash = `#/trace/${h.trace}` }}
  onmousemove={(e) => { canvas.style.cursor = near(e) ? 'pointer' : 'default' }}
></canvas>

<style>
  canvas { width: 100%; height: 320px; display: block; margin-top: 12px; }
</style>
