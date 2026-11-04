// Both Helides fetch their latest release: the Helix fork, and the first one, the Windows app.
// Without the API (offline, rate-limited) the links fall back to the releases pages and the
// versions already written into the page stay.
function formatSize(bytes) {
  if (!bytes) return null;
  const mb = bytes / 1048576;
  return mb >= 1 ? mb.toFixed(0) + " MB" : (bytes / 1024).toFixed(0) + " KB";
}

async function latest(repo) {
  const res = await fetch("https://api.github.com/repos/" + repo + "/releases/latest", {
    headers: { Accept: "application/vnd.github+json" },
  });
  if (!res.ok) throw new Error("release not found");
  return res.json();
}

latest("ujjwalvivek/helide-rs")
  .then(release => {
    const tag = release.tag_name || "";
    if (tag) document.querySelectorAll(".js-version").forEach(el => (el.textContent = tag));
    // The plain Linux build; the -otari one is the hackathon twin.
    const asset = (release.assets || []).find(
      a => a.name.endsWith("-x86_64-linux.tar.xz") && !a.name.includes("-otari"),
    );
    if (asset) {
      document.getElementById("download").href = asset.browser_download_url;
      const size = formatSize(asset.size);
      const sizeEl = document.querySelector(".js-size");
      if (size && sizeEl) sizeEl.textContent = size;
    }
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
    if (asset) document.getElementById("first-download").href = asset.browser_download_url;
  })
  .catch(() => {});

// Screenshots carousel
const SHOTS = [
  { id: "hero", label: "workspace" },
  { id: "git", label: "git panel" },
  { id: "splits", label: "tabs per split" },
  { id: "history", label: "time travel" },
  { id: "preview", label: "markdown preview" },
];

// The first Helide's screenshots, under its own window.
const FIRST_SHOTS = [
  { id: "workspace", label: "the first helide: workspace" },
  { id: "filetree", label: "the first helide: file tree" },
  { id: "zen", label: "the first helide: zen mode" },
];

const carousel = document.getElementById("carousel");
const track = document.getElementById("carousel-track");
const dotsBox = document.createElement("div"); // the reel's dots are not shown
const lightbox = document.getElementById("lightbox");

if (carousel && track && dotsBox && lightbox && SHOTS.length) {
  const thumb = id => "images/thumbnails/" + id + "-640x360.webp";
  const full = id => "images/webp/" + id + "-1920x1080.webp";

  const SPEED = 0.014; // px per ms
  const reduceMotion =
    window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  // Below 720px the sidebar stacks full width, so the reel scrolls sideways
  // instead of downwards. This mirrors the CSS breakpoint.
  const axisQuery = window.matchMedia("(max-width: 720px)");
  let horizontal = axisQuery.matches;

  let offset = 0;
  let stride = 0;
  let last = 0;
  let active = -1;
  let current = 0;
  let lastFocus = null;
  let hovering = false;
  let gliding = 0;
  let lightboxOpen = false;

  const slides = [];

  // Keeps the track offset in [-loop, 0) so a full period is one loop wide.
  function wrap(v, loop) {
    if (!(loop > 0)) return 0;
    return ((v % loop) + loop) % loop - loop;
  }

  function place() {
    track.style.transform = horizontal
      ? "translate3d(" + offset + "px,0,0)"
      : "translate3d(0," + offset + "px,0)";

    carousel.style.setProperty("--film", offset + "px");
  }

  function makeSlide(shot, index, hidden) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "carousel-slide";
    if (hidden) btn.setAttribute("aria-hidden", "true");
    btn.tabIndex = hidden ? -1 : 0;
    btn.dataset.index = index;

    const img = document.createElement("img");
    img.src = thumb(shot.id);
    img.alt = shot.label;
    // The reel duplicates every slide, so lazy images leave blank frames until
    // the transform scrolls them near the viewport. The set is tiny, so load it
    // eagerly to keep the loop seamless from the first frame.
    img.loading = "eager";
    img.decoding = "sync";
    img.draggable = false;

    const cap = document.createElement("span");
    cap.className = "slide-caption";
    cap.textContent = shot.label;

    btn.append(img, cap);
    return btn;
  }

  function build() {
    const frag = document.createDocumentFragment();
    slides.length = 0;

    SHOTS.forEach(shot => {
      const pre = new Image();
      pre.src = thumb(shot.id);
    });

    SHOTS.forEach((shot, i) => {
      const btn = makeSlide(shot, i, false);
      slides.push(btn);
      frag.appendChild(btn);
    });

    track.appendChild(frag);
    ensureCopies();

    dotsBox.innerHTML = "";
    SHOTS.forEach((shot, i) => {
      const dot = document.createElement("button");
      dot.type = "button";
      dot.className = "dot";
      dot.setAttribute("role", "tab");
      dot.setAttribute("aria-label", shot.label);
      dot.setAttribute("aria-selected", i === 0 ? "true" : "false");
      dot.addEventListener("click", () => goTo(i));
      dotsBox.appendChild(dot);
    });
  }

  // The offset wraps once per period (loop = stride * SHOTS.length). To keep the
  // loop seamless the track must stay at least one period taller than the
  // viewport, otherwise wrapping exposes an empty band. Append hidden copies
  // until that holds, with a cap so a huge viewport can't grow forever.
  function ensureCopies() {
    if (!stride) {
      // stride is not measured yet; add one copy so measure() can run.
      if (!track.children.length || track.children.length < SHOTS.length * 2) {
        const frag = document.createDocumentFragment();
        SHOTS.forEach((shot, i) => frag.appendChild(makeSlide(shot, i, true)));
        track.appendChild(frag);
      }
      return;
    }
    const loop = stride * SHOTS.length;
    const viewport = horizontal ? carousel.clientWidth : carousel.clientHeight;
    const need = loop + viewport + stride;
    const copies = Math.ceil(track.children.length / SHOTS.length);
    const maxCopies = Math.min(24, Math.ceil(need / loop) + 2);
    if (copies < maxCopies) {
      const frag = document.createDocumentFragment();
      for (let c = copies; c < maxCopies; c++) {
        SHOTS.forEach((shot, i) => frag.appendChild(makeSlide(shot, i, true)));
      }
      track.appendChild(frag);
    }
  }

  function measure() {
    if (!slides.length) return;
    const first = slides[0];
    const next = slides[1];
    const gap = parseFloat(
      getComputedStyle(track)[horizontal ? "columnGap" : "rowGap"]
    ) || 0;
    stride = next
      ? (horizontal
          ? first.offsetWidth +
            (next.getBoundingClientRect().left - first.getBoundingClientRect().right)
          : first.offsetHeight +
            (next.getBoundingClientRect().top - first.getBoundingClientRect().bottom))
      : (horizontal ? first.offsetWidth : first.offsetHeight) + gap;
    ensureCopies();
    offset = wrap(offset, stride * SHOTS.length);
    place();
  }

  // Eases the track to whichever copy of slide i sits nearest the viewport,
  // so a dot click never jumps a whole period.
  function goTo(i) {
    current = ((i % SHOTS.length) + SHOTS.length) % SHOTS.length;
    const loop = stride * SHOTS.length;
    const box = horizontal ? carousel.clientWidth : carousel.clientHeight;
    const itemSize = horizontal
      ? slides[current].offsetWidth
      : slides[current].offsetHeight;
    const target = -(current * stride) + (box - itemSize) / 2;
    offset = wrap(target + Math.round((offset - target) / loop) * loop, loop);

    if (reduceMotion) {
      place();
    } else {
      gliding = performance.now() + 420;
      track.classList.add("is-gliding");
      place();
    }
    setActive(current);
  }

  function setActive(i) {
    if (i === active) return;
    active = i;
    const dots = dotsBox.children;
    for (let k = 0; k < dots.length; k++) {
      dots[k].setAttribute("aria-selected", k === i ? "true" : "false");
    }
  }

  function nearestToCenter() {
    const boxRect = carousel.getBoundingClientRect();
    const mid = horizontal ? boxRect.left + boxRect.width / 2 : boxRect.top + boxRect.height / 2;
    let best = 0;
    let bestDist = Infinity;
    for (let k = 0; k < slides.length; k++) {
      const r = slides[k].getBoundingClientRect();
      const c = horizontal ? r.left + r.width / 2 : r.top + r.height / 2;
      const d = Math.abs(c - mid);
      if (d < bestDist) {
        bestDist = d;
        best = slides[k].dataset.index | 0;
      }
    }
    return best;
  }

  function tick(now) {
    if (!last) last = now;
    const dt = Math.min(now - last, 100);
    last = now;

    if (gliding && now > gliding) {
      gliding = 0;
      track.classList.remove("is-gliding");
    }

    const frozen = reduceMotion || hovering || lightboxOpen || gliding || document.hidden;
    if (!frozen && stride > 0) {
      offset = wrap(offset - dt * SPEED, stride * SHOTS.length);
      place();
      setActive(nearestToCenter());
    }
    requestAnimationFrame(tick);
  }

  track.addEventListener("click", e => {
    const slide = e.target.closest(".carousel-slide");
    if (slide) open(slide.dataset.index | 0);
  });

  carousel.addEventListener("mouseenter", () => {
    hovering = true;
  });
  carousel.addEventListener("mouseleave", () => {
    hovering = false;
  });

  let resizeTimer;
  function scheduleMeasure() {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(measure, 120);
  }

  window.addEventListener("resize", scheduleMeasure);

  const onAxisChange = () => {
    horizontal = axisQuery.matches;
    scheduleMeasure();
  };
  if (axisQuery.addEventListener) axisQuery.addEventListener("change", onAxisChange);
  else axisQuery.addListener(onAxisChange);

  // Lightbox
  const lbImg = document.getElementById("lightbox-img");
  const lbLabel = document.getElementById("lightbox-label");
  const lbIndex = document.getElementById("lightbox-index");

  // The lightbox shows one list at a time: the reel's, or the first Helide's.
  let list = SHOTS;
  let src = full;
  const firstFull = id => "images/windows/" + id + "-1920x1080.webp";

  function preload(i) {
    const shot = list[((i % list.length) + list.length) % list.length];
    const img = new Image();
    img.src = src(shot.id);
  }

  function open(i, first) {
    if (lightboxOpen) return;
    lightboxOpen = true;
    list = first ? FIRST_SHOTS : SHOTS;
    src = first ? firstFull : full;
    current = ((i % list.length) + list.length) % list.length;
    lastFocus = document.activeElement;
    render();
    preload(current + 1);
    preload(current - 1);
    lightbox.hidden = false;
    document.body.style.overflow = "hidden";
    document.getElementById("lightbox-close").focus();
  }

  function render() {
    const shot = list[current];
    lbImg.src = src(shot.id);
    lbImg.alt = shot.label;
    lbLabel.textContent = shot.label;
    lbIndex.textContent = (current + 1) + " / " + list.length;
  }

  function step(dir) {
    current = (current + dir + list.length) % list.length;
    render();
    preload(current + dir);
    if (list === SHOTS) goTo(current);
  }

  function close() {
    if (!lightboxOpen) return;
    lightboxOpen = false;
    lightbox.hidden = true;
    document.body.style.overflow = "";
    lbImg.src = "";
    if (lastFocus && lastFocus.focus) lastFocus.focus();
    if (list === SHOTS) goTo(current);
  }

  lightbox.addEventListener("click", e => {
    if (e.target.hasAttribute("data-lightbox-close")) close();
    else if (e.target.id === "lightbox-prev") step(-1);
    else if (e.target.id === "lightbox-next") step(1);
    else if (e.target.id === "lightbox-close") close();
  });

  document.addEventListener("keydown", e => {
    if (!lightboxOpen) return;
    if (e.key === "Escape") close();
    else if (e.key === "ArrowLeft") step(-1);
    else if (e.key === "ArrowRight") step(1);
    else if (e.key === "Tab") e.preventDefault();
  });

  // The first Helide's screenshots: three thumbnails that open the same viewer.
  const firstBox = document.getElementById("first-shots");
  if (firstBox) {
    FIRST_SHOTS.forEach((shot, i) => {
      const btn = document.createElement("button");
      btn.type = "button";
      const img = document.createElement("img");
      img.src = "images/windows/" + shot.id + "-640x360.webp";
      img.alt = shot.label;
      img.loading = "eager";
      btn.appendChild(img);
      btn.addEventListener("click", () => open(i, true));
      firstBox.appendChild(btn);
    });
  }

  build();
  requestAnimationFrame(() => {
    measure();
    requestAnimationFrame(tick);
  });
}

// Background shader, and the glass rim on the window.
//
// One shader file, built twice. The background is a full-page canvas behind
// everything. The rim is a second canvas laid over the window, which paints
// only the window's own border: see background.glsl.
(function () {
  const bgCanvas = document.getElementById("bg-shader");
  const rimCanvas = document.getElementById("rim-shader");
  const winEl = document.getElementById("window");
  if (!bgCanvas || !bgCanvas.getContext) return;

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

  // Builds one of the two programs on a canvas. Returns null when WebGL is not
  // there or the shader will not compile, and the page simply goes without.
  function build(canvas, source, defines, options) {
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

  fetch("background.glsl")
    .then(r => r.text())
    .then(source => {
      const bg = build(bgCanvas, source, "", { preserveDrawingBuffer: true });
      if (!bg) return;

      // The rim blends over the window, so its canvas keeps an alpha channel.
      const rim = rimCanvas && winEl
        ? build(rimCanvas, source, "#define RIM\n", { alpha: true, premultipliedAlpha: true })
        : null;

      // Drawn at the screen's own resolution, up to 2x: the highlight on the
      // rim is about a pixel wide, and upscaled it turns into a soft band.
      let scale = 1;
      function fit(canvas, width, height) {
        const w = Math.max(1, Math.round(width * scale));
        const h = Math.max(1, Math.round(height * scale));
        if (canvas.width !== w || canvas.height !== h) {
          canvas.width = w;
          canvas.height = h;
        }
      }

      // Listened for on the window object: the background canvas is behind the
      // page and never sees the pointer. Starts in the middle so the highlight
      // has somewhere sensible to be before the pointer has moved.
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

        // Sized to the canvas as laid out, not to the window: it is deliberately
        // taller than the screen, and on a phone innerHeight is not even the
        // same thing from one moment to the next as the toolbars come and go.
        const view = bgCanvas.getBoundingClientRect();
        fit(bgCanvas, view.width || window.innerWidth, view.height || window.innerHeight);
        bg.gl.viewport(0, 0, bgCanvas.width, bgCanvas.height);
        bg.gl.uniform1f(bg.iTime, t);
        bg.gl.uniform2f(bg.iResolution, bgCanvas.width, bgCanvas.height);
        bg.gl.uniform2f(bg.iMouse, mouse[0], mouse[1]);
        bg.gl.drawArrays(bg.gl.TRIANGLES, 0, 3);

        if (rim) {
          // The rim has to sample the background exactly where the background
          // drew it, so it is told the page's resolution and where on the page
          // the window's bottom-left corner is, both in background pixels.
          const r = winEl.getBoundingClientRect();
          const band = parseFloat(getComputedStyle(winEl).getPropertyValue("--rim")) || 12;
          const gl = rim.gl;

          fit(rimCanvas, r.width, r.height);
          gl.viewport(0, 0, rimCanvas.width, rimCanvas.height);
          gl.clearColor(0, 0, 0, 0);
          gl.clear(gl.COLOR_BUFFER_BIT);
          gl.uniform1f(rim.iTime, t);
          gl.uniform2f(rim.iResolution, bgCanvas.width, bgCanvas.height);
          gl.uniform2f(rim.iMouse, mouse[0], mouse[1]);
          gl.uniform2f(rim.uSize, rimCanvas.width, rimCanvas.height);
          gl.uniform2f(rim.uOrigin, (r.left - view.left) * scale, (view.bottom - r.bottom) * scale);
          gl.uniform1f(rim.uBand, band * scale);
          gl.uniform1f(rim.uScale, scale);
          gl.drawArrays(gl.TRIANGLES, 0, 3);
        }

        requestAnimationFrame(draw);
      }
      draw();
    })
    .catch(err => console.warn("Shader load failed:", err));
})();
