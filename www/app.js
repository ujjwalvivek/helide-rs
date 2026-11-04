// The site is three pages that share one window: helide, docs, and helide for windows. The
// tabs swap what is inside the window without reloading, so the background shader keeps
// running and never drops to black between pages. Each page still loads on its own when
// opened directly.

// ---------------------------------------------------------------------------
// Releases: both Helides fetch their latest one, once per visit. Without the API (offline,
// rate-limited) the links keep pointing at the releases pages and the written versions stay.

const releases = {};

function latest(repo) {
  if (!releases[repo]) {
    releases[repo] = fetch("https://api.github.com/repos/" + repo + "/releases/latest", {
      headers: { Accept: "application/vnd.github+json" },
    }).then(res => (res.ok ? res.json() : Promise.reject(new Error("no release"))));
  }
  return releases[repo];
}

function formatSize(bytes) {
  if (!bytes) return null;
  const mb = bytes / 1048576;
  return mb >= 1 ? mb.toFixed(0) + " MB" : (bytes / 1024).toFixed(0) + " KB";
}

function showReleases() {
  latest("ujjwalvivek/helide-rs")
    .then(release => {
      const tag = release.tag_name || "";
      if (tag) document.querySelectorAll(".js-version").forEach(el => (el.textContent = tag));
      // The plain Linux build; the -otari one is the hackathon twin.
      const asset = (release.assets || []).find(
        a => a.name.endsWith("-x86_64-linux.tar.xz") && !a.name.includes("-otari"),
      );
      const link = document.getElementById("download");
      if (asset && link) link.href = asset.browser_download_url;
      const sizeEl = document.querySelector(".js-size");
      const size = asset && formatSize(asset.size);
      if (size && sizeEl) sizeEl.textContent = size;
    })
    .catch(() => {});

  latest("ujjwalvivek/helide")
    .then(release => {
      const tag = release.tag_name || "";
      if (tag)
        document
          .querySelectorAll(".js-first-version")
          .forEach(el => (el.textContent = tag.startsWith("v") ? tag : "v" + tag));
      const asset = (release.assets || []).find(a => a.name === "helide-win-x64.zip");
      const link = document.getElementById("first-download");
      if (asset && link) link.href = asset.browser_download_url;
    })
    .catch(() => {});
}

// ---------------------------------------------------------------------------
// Screenshots

const SHOTS = [
  { id: "hero", label: "workspace" },
  { id: "git", label: "git panel" },
  { id: "splits", label: "tabs per split" },
  { id: "history", label: "time travel" },
  { id: "preview", label: "markdown preview" },
];
const shotThumb = id => "images/thumbnails/" + id + "-640x360.webp";
const shotFull = id => "images/webp/" + id + "-1920x1080.webp";

// The first Helide's, on its own page.
const FIRST_SHOTS = [
  { id: "workspace", label: "workspace" },
  { id: "filetree", label: "file tree" },
  { id: "zen", label: "zen mode" },
];
const firstThumb = id => "images/windows/" + id + "-640x360.webp";
const firstFull = id => "images/windows/" + id + "-1920x1080.webp";

const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

// ---------------------------------------------------------------------------
// Lightbox: one for the whole site, showing whichever list it was opened with.

const viewer = (() => {
  const box = document.getElementById("lightbox");
  if (!box) return { open() {}, isOpen: () => false };
  const img = document.getElementById("lightbox-img");
  const label = document.getElementById("lightbox-label");
  const index = document.getElementById("lightbox-index");
  let list = SHOTS;
  let full = shotFull;
  let current = 0;
  let onMove = null;
  let lastFocus = null;
  let open = false;

  const wrap = i => ((i % list.length) + list.length) % list.length;

  function preload(i) {
    new Image().src = full(list[wrap(i)].id);
  }

  function render() {
    const shot = list[current];
    img.src = full(shot.id);
    img.alt = shot.label;
    label.textContent = shot.label;
    index.textContent = current + 1 + " / " + list.length;
  }

  function step(dir) {
    current = wrap(current + dir);
    render();
    preload(current + dir);
    if (onMove) onMove(current);
  }

  function close() {
    if (!open) return;
    open = false;
    box.hidden = true;
    document.body.style.overflow = "";
    img.src = "";
    if (lastFocus && lastFocus.focus) lastFocus.focus();
    if (onMove) onMove(current);
  }

  box.addEventListener("click", e => {
    if (e.target.hasAttribute("data-lightbox-close")) close();
    else if (e.target.id === "lightbox-prev") step(-1);
    else if (e.target.id === "lightbox-next") step(1);
    else if (e.target.id === "lightbox-close") close();
  });

  document.addEventListener("keydown", e => {
    if (!open) return;
    if (e.key === "Escape") close();
    else if (e.key === "ArrowLeft") step(-1);
    else if (e.key === "ArrowRight") step(1);
    else if (e.key === "Tab") e.preventDefault();
  });

  return {
    // Opens `shots` at `i`; `moved` hears where the viewer ended up, so a reel can follow.
    open(shots, fullOf, i, moved) {
      if (open) return;
      open = true;
      list = shots;
      full = fullOf;
      onMove = moved || null;
      current = wrap(i);
      lastFocus = document.activeElement;
      render();
      preload(current + 1);
      preload(current - 1);
      box.hidden = false;
      document.body.style.overflow = "hidden";
      document.getElementById("lightbox-close").focus();
    },
    isOpen: () => open,
  };
})();

// ---------------------------------------------------------------------------
// The reel: the screenshots scrolling past in the sidebar, as on a film strip. Started for
// each page that has one; returns a function that stops it.

function startReel(carousel, track) {
  const SPEED = 0.014; // px per ms
  // Below 720px the sidebar stacks full width, so the reel scrolls sideways instead of
  // downwards. This mirrors the CSS breakpoint.
  const axisQuery = window.matchMedia("(max-width: 720px)");
  let horizontal = axisQuery.matches;
  let offset = 0;
  let stride = 0;
  let last = 0;
  let hovering = false;
  let gliding = 0;
  let alive = true;
  const slides = [];

  // Keeps the offset in [-loop, 0) so a full period is one loop wide.
  function wrap(v, loop) {
    if (!(loop > 0)) return 0;
    return (((v % loop) + loop) % loop) - loop;
  }

  function place() {
    track.style.transform = horizontal
      ? "translate3d(" + offset + "px,0,0)"
      : "translate3d(0," + offset + "px,0)";
    carousel.style.setProperty("--film", offset + "px");
  }

  function makeSlide(shot, i, hidden) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "carousel-slide";
    if (hidden) btn.setAttribute("aria-hidden", "true");
    btn.tabIndex = hidden ? -1 : 0;
    btn.dataset.index = i;
    const img = document.createElement("img");
    img.src = shotThumb(shot.id);
    img.alt = shot.label;
    // The reel repeats every slide, so lazy images would leave blank frames as it loops.
    img.loading = "eager";
    img.decoding = "sync";
    img.draggable = false;
    const cap = document.createElement("span");
    cap.className = "slide-caption";
    cap.textContent = shot.label;
    btn.append(img, cap);
    return btn;
  }

  // The track must stay at least one period longer than the viewport, or wrapping shows an
  // empty band; hidden copies are added until it is, with a cap.
  function ensureCopies() {
    if (!stride) {
      if (track.children.length < SHOTS.length * 2) {
        SHOTS.forEach((shot, i) => track.appendChild(makeSlide(shot, i, true)));
      }
      return;
    }
    const loop = stride * SHOTS.length;
    const viewport = horizontal ? carousel.clientWidth : carousel.clientHeight;
    const copies = Math.ceil(track.children.length / SHOTS.length);
    const wanted = Math.min(24, Math.ceil((loop + viewport + stride) / loop) + 2);
    for (let c = copies; c < wanted; c++) {
      SHOTS.forEach((shot, i) => track.appendChild(makeSlide(shot, i, true)));
    }
  }

  function measure() {
    if (!alive || slides.length < 2) return;
    const a = slides[0].getBoundingClientRect();
    const b = slides[1].getBoundingClientRect();
    stride = horizontal ? b.left - a.left : b.top - a.top;
    ensureCopies();
    offset = wrap(offset, stride * SHOTS.length);
    place();
  }

  // Eases the track to whichever copy of slide i sits nearest, so it never jumps a period.
  function goTo(i) {
    if (!stride) return;
    const loop = stride * SHOTS.length;
    const box = horizontal ? carousel.clientWidth : carousel.clientHeight;
    const size = horizontal ? slides[i].offsetWidth : slides[i].offsetHeight;
    const target = -(i * stride) + (box - size) / 2;
    offset = wrap(target + Math.round((offset - target) / loop) * loop, loop);
    if (!reduceMotion) {
      gliding = performance.now() + 420;
      track.classList.add("is-gliding");
    }
    place();
  }

  function tick(now) {
    if (!alive) return;
    if (!last) last = now;
    const dt = Math.min(now - last, 100);
    last = now;
    if (gliding && now > gliding) {
      gliding = 0;
      track.classList.remove("is-gliding");
    }
    const frozen = reduceMotion || hovering || viewer.isOpen() || gliding || document.hidden;
    if (!frozen && stride > 0) {
      offset = wrap(offset - dt * SPEED, stride * SHOTS.length);
      place();
    }
    requestAnimationFrame(tick);
  }

  SHOTS.forEach((shot, i) => {
    const btn = makeSlide(shot, i, false);
    slides.push(btn);
    track.appendChild(btn);
  });
  ensureCopies();

  track.addEventListener("click", e => {
    const slide = e.target.closest(".carousel-slide");
    if (slide) viewer.open(SHOTS, shotFull, slide.dataset.index | 0, goTo);
  });
  carousel.addEventListener("mouseenter", () => (hovering = true));
  carousel.addEventListener("mouseleave", () => (hovering = false));

  let timer;
  const onResize = () => {
    clearTimeout(timer);
    timer = setTimeout(measure, 120);
  };
  const onAxis = () => {
    horizontal = axisQuery.matches;
    onResize();
  };
  window.addEventListener("resize", onResize);
  axisQuery.addEventListener("change", onAxis);

  requestAnimationFrame(() => {
    measure();
    requestAnimationFrame(tick);
  });

  return () => {
    alive = false;
    window.removeEventListener("resize", onResize);
    axisQuery.removeEventListener("change", onAxis);
  };
}

function showFirstShots() {
  const box = document.getElementById("first-shots");
  if (!box) return;
  FIRST_SHOTS.forEach((shot, i) => {
    const btn = document.createElement("button");
    btn.type = "button";
    const img = document.createElement("img");
    img.src = firstThumb(shot.id);
    img.alt = shot.label;
    btn.appendChild(img);
    btn.addEventListener("click", () => viewer.open(FIRST_SHOTS, firstFull, i));
    box.appendChild(btn);
  });
}

// ---------------------------------------------------------------------------
// Background shader, and the glass rim on the window.
//
// One shader file, built twice. The background is a full-page canvas behind everything and
// is built once. The rim is a canvas inside the window, which paints only the window's own
// border (see background.glsl); it is rebuilt whenever the window's contents are swapped.

const shader = (() => {
  const bgCanvas = document.getElementById("bg-shader");
  let source = null;
  let bg = null;
  let rim = null;

  const VS = "attribute vec2 a_pos; void main() { gl_Position = vec4(a_pos, 0.0, 1.0); }";
  const HEADER = [
    "#ifdef GL_FRAGMENT_PRECISION_HIGH",
    "precision highp float;",
    "#else",
    "precision mediump float;",
    "#endif",
    "uniform float iTime;",
    "uniform vec2 iResolution;",
    "uniform vec2 iMouse;",
    "uniform vec2 uSize;",
    "uniform vec2 uOrigin;",
    "uniform float uBand;",
    "uniform float uScale;",
    "",
  ].join("\n");

  // Builds a program on a canvas, or returns null when WebGL is missing or the shader will
  // not compile; the page simply goes without.
  function build(canvas, defines, options) {
    const gl = canvas.getContext("webgl", options) || canvas.getContext("experimental-webgl", options);
    if (!gl) return null;
    function compile(type, src) {
      const sh = gl.createShader(type);
      gl.shaderSource(sh, src);
      gl.compileShader(sh);
      if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) {
        console.warn(gl.getShaderInfoLog(sh));
        return null;
      }
      return sh;
    }
    const vs = compile(gl.VERTEX_SHADER, VS);
    const fs = compile(gl.FRAGMENT_SHADER, defines + HEADER + source);
    if (!vs || !fs) return null;
    const prog = gl.createProgram();
    gl.attachShader(prog, vs);
    gl.attachShader(prog, fs);
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) {
      console.warn(gl.getProgramInfoLog(prog));
      return null;
    }
    gl.useProgram(prog);
    const pos = gl.getAttribLocation(prog, "a_pos");
    gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
    gl.enableVertexAttribArray(pos);
    gl.vertexAttribPointer(pos, 2, gl.FLOAT, false, 0, 0);
    const at = name => gl.getUniformLocation(prog, name);
    return {
      gl,
      canvas,
      iTime: at("iTime"),
      iResolution: at("iResolution"),
      iMouse: at("iMouse"),
      uSize: at("uSize"),
      uOrigin: at("uOrigin"),
      uBand: at("uBand"),
      uScale: at("uScale"),
    };
  }

  // The rim, on the current window's canvas.
  function attachRim() {
    rim = null;
    const canvas = document.getElementById("rim-shader");
    const win = document.getElementById("window");
    if (!source || !canvas || !win) return;
    const built = build(canvas, "#define RIM\n", { alpha: true, premultipliedAlpha: true });
    if (built) rim = Object.assign(built, { win });
  }

  // Drawn at the screen's own resolution, up to 2x: the highlight on the rim is about a
  // pixel wide, and upscaled it turns into a soft band.
  let scale = 1;
  function fit(canvas, width, height) {
    const w = Math.max(1, Math.round(width * scale));
    const h = Math.max(1, Math.round(height * scale));
    if (canvas.width !== w || canvas.height !== h) {
      canvas.width = w;
      canvas.height = h;
    }
  }

  // The background canvas is behind the page and never sees the pointer, so it is listened
  // for on the window. Starts in the middle, before the pointer has moved.
  const mouse = [0.5, 0.5];
  window.addEventListener("pointermove", e => {
    const view = bgCanvas.getBoundingClientRect();
    if (!view.width || !view.height) return;
    mouse[0] = (e.clientX - view.left) / view.width;
    mouse[1] = 1.0 - (e.clientY - view.top) / view.height;
  });

  const start = performance.now();

  function draw() {
    const t = (performance.now() - start) / 1000;
    scale = Math.min(window.devicePixelRatio || 1, 2);
    // Sized to the canvas as laid out: it is deliberately taller than the screen, and on a
    // phone innerHeight changes as the toolbars come and go.
    const view = bgCanvas.getBoundingClientRect();
    fit(bgCanvas, view.width || window.innerWidth, view.height || window.innerHeight);
    bg.gl.viewport(0, 0, bgCanvas.width, bgCanvas.height);
    bg.gl.uniform1f(bg.iTime, t);
    bg.gl.uniform2f(bg.iResolution, bgCanvas.width, bgCanvas.height);
    bg.gl.uniform2f(bg.iMouse, mouse[0], mouse[1]);
    bg.gl.drawArrays(bg.gl.TRIANGLES, 0, 3);

    if (rim && rim.win.isConnected) {
      // The rim samples the background exactly where it was drawn, so it is told the page's
      // resolution and where the window's bottom-left corner is, in background pixels.
      const r = rim.win.getBoundingClientRect();
      const band = parseFloat(getComputedStyle(rim.win).getPropertyValue("--rim")) || 12;
      const gl = rim.gl;
      fit(rim.canvas, r.width, r.height);
      gl.viewport(0, 0, rim.canvas.width, rim.canvas.height);
      gl.clearColor(0, 0, 0, 0);
      gl.clear(gl.COLOR_BUFFER_BIT);
      gl.uniform1f(rim.iTime, t);
      gl.uniform2f(rim.iResolution, bgCanvas.width, bgCanvas.height);
      gl.uniform2f(rim.iMouse, mouse[0], mouse[1]);
      gl.uniform2f(rim.uSize, rim.canvas.width, rim.canvas.height);
      gl.uniform2f(rim.uOrigin, (r.left - view.left) * scale, (view.bottom - r.bottom) * scale);
      gl.uniform1f(rim.uBand, band * scale);
      gl.uniform1f(rim.uScale, scale);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
    }
    requestAnimationFrame(draw);
  }

  if (bgCanvas && bgCanvas.getContext) {
    fetch("background.glsl")
      .then(r => r.text())
      .then(text => {
        source = text;
        bg = build(bgCanvas, "", { preserveDrawingBuffer: true });
        if (!bg) return;
        attachRim();
        draw();
      })
      .catch(err => console.warn("Shader load failed:", err));
  }

  return { attachRim };
})();

// ---------------------------------------------------------------------------
// Pages: what each one needs once its window is in place.

let stopReel = null;

function setUpPage() {
  if (stopReel) stopReel();
  stopReel = null;
  const carousel = document.getElementById("carousel");
  const track = document.getElementById("carousel-track");
  if (carousel && track) stopReel = startReel(carousel, track);
  showFirstShots();
  showReleases();
}

// Tabs and other links between the pages swap the window in place. Anything else (other
// sites, new tabs, modified clicks) is left to the browser.
const PAGES = ["", "index.html", "docs.html", "windows.html"];

function pageOf(url) {
  if (url.origin !== location.origin) return null;
  const name = url.pathname.split("/").pop();
  return PAGES.includes(name) ? name : null;
}

async function go(url, push) {
  let html;
  try {
    const res = await fetch(url.pathname);
    if (!res.ok) throw new Error(res.status);
    html = await res.text();
  } catch {
    location.href = url.href;
    return;
  }
  const next = new DOMParser().parseFromString(html, "text/html");
  const main = next.querySelector("main");
  if (!main) {
    location.href = url.href;
    return;
  }
  document.querySelector("main").replaceWith(document.adoptNode(main));
  document.title = next.title;
  document.body.className = next.body.className;
  if (push) history.pushState(null, "", url.href);
  setUpPage();
  shader.attachRim();
  const target = url.hash && document.getElementById(url.hash.slice(1));
  if (target) target.scrollIntoView();
  else window.scrollTo(0, 0);
}

document.addEventListener("click", e => {
  if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
  const a = e.target.closest("a[href]");
  if (!a || a.target) return;
  const url = new URL(a.href, location.href);
  if (pageOf(url) === null) return;
  // A link to a place on the same page just scrolls.
  if (url.pathname === location.pathname && url.hash) return;
  e.preventDefault();
  go(url, true);
});

window.addEventListener("popstate", () => go(new URL(location.href), false));

setUpPage();
