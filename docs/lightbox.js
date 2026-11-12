// A larger view of the screenshots. Every link with the class "zoom" points at its
// image, so without this script a click still opens the picture on its own.
(() => {
  const links = [...document.querySelectorAll('a.zoom')];
  if (!links.length || typeof HTMLDialogElement === 'undefined') return;

  // What to say under each picture: the link's own words, or its figure's caption.
  const shots = links.map((link) => {
    const figure = link.closest('figure');
    const heading = figure && figure.querySelector('figcaption h3');
    const text = figure && figure.querySelector('figcaption p:not(.shot-new)');
    const img = link.querySelector('img');
    return {
      link,
      src: link.getAttribute('href'),
      alt: img ? img.alt : '',
      title: link.dataset.title || (heading ? heading.textContent : ''),
      caption: link.dataset.caption || (text ? text.textContent : ''),
    };
  });

  // The larger view brings its own styles, so a browser holding an older copy of
  // the site's stylesheet cannot show the picture at full size with scroll bars.
  const style = document.createElement('style');
  style.textContent = `
    .lightbox { box-sizing: border-box; width: 100vw; max-width: none; height: 100vh; height: 100dvh; max-height: none; margin: 0; padding: clamp(.75rem, 2.5vw, 2rem); border: 0; background: none; color: #f5f1e7; overflow: hidden; }
    .lightbox[open] { display: grid; grid-template-rows: minmax(0, 1fr) auto; gap: 1rem; animation: lightbox-in .2s ease; }
    .lightbox::backdrop { background: #05080ccc; -webkit-backdrop-filter: blur(14px); backdrop-filter: blur(14px); }
    .lightbox-stage { display: grid; grid-template-columns: auto minmax(0, 1fr) auto; gap: clamp(.25rem, 1.5vw, 1rem); align-items: center; min-height: 0; cursor: zoom-out; }
    .lightbox-image { display: block; max-width: 100%; max-height: 100%; width: auto; height: auto; margin: auto; border: 1px solid #ffffff24; border-radius: .9rem; box-shadow: 0 2rem 6rem #000c; cursor: zoom-out; object-fit: contain; min-height: 0; }
    .lightbox-words { display: flex; gap: 1.5rem; align-items: end; justify-content: space-between; width: min(100%, 70rem); margin: 0 auto; }
    .lightbox-title { margin: 0 0 .3rem; font-size: clamp(1.05rem, 2vw, 1.4rem); letter-spacing: -.02em; }
    .lightbox-caption { max-width: 52rem; margin: 0; color: #c9c5bb; font-size: .95rem; line-height: 1.5; }
    .lightbox-count { margin: 0; color: #aaa79f; font-family: ui-monospace, monospace; font-size: .8rem; white-space: nowrap; }
    .lightbox button { display: grid; place-items: center; width: 2.75rem; height: 2.75rem; padding: 0 0 .15rem; border: 1px solid #ffffff2e; border-radius: 50%; background: #0d131ae6; color: #f5f1e7; font: inherit; font-size: 1.6rem; line-height: 1; cursor: pointer; }
    .lightbox button:hover, .lightbox button:focus-visible { border-color: var(--accent); color: var(--accent); outline: none; }
    .lightbox-close { position: absolute; top: clamp(.75rem, 2vw, 1.5rem); right: clamp(.75rem, 2vw, 1.5rem); z-index: 1; }
    @keyframes lightbox-in { from { opacity: 0; transform: scale(.985); } }
    @media (max-width: 46rem) {
      .lightbox-stage { grid-template-columns: minmax(0, 1fr); }
      .lightbox-step { position: absolute; bottom: clamp(.75rem, 2.5vw, 2rem); z-index: 1; }
      .lightbox-previous { right: 4.25rem; }
      .lightbox-next { right: clamp(.75rem, 2.5vw, 2rem); }
      .lightbox-words { display: block; padding-right: 7.5rem; }
      .lightbox-caption { font-size: .85rem; }
      .lightbox-count { margin-top: .4rem; }
    }
  `;
  document.head.append(style);

  const make = (tag, props = {}, ...children) => {
    const node = Object.assign(document.createElement(tag), props);
    node.append(...children);
    return node;
  };
  const picture = make('img', { className: 'lightbox-image', alt: '' });
  const title = make('h2', { className: 'lightbox-title', id: 'lightbox-title' });
  const caption = make('p', { className: 'lightbox-caption' });
  const count = make('p', { className: 'lightbox-count' });
  count.setAttribute('aria-live', 'polite');
  const button = (className, label, text) => {
    const b = make('button', { type: 'button', className, textContent: text });
    b.setAttribute('aria-label', label);
    return b;
  };
  const close = button('lightbox-close', 'Close the larger view', '×');
  const previous = button('lightbox-step lightbox-previous', 'Previous screenshot', '‹');
  const next = button('lightbox-step lightbox-next', 'Next screenshot', '›');
  const stage = make('div', { className: 'lightbox-stage' }, previous, picture, next);
  const words = make('div', { className: 'lightbox-words' }, make('div', {}, title, caption), count);
  const dialog = make('dialog', { className: 'lightbox' }, close, stage, words);
  dialog.setAttribute('aria-labelledby', 'lightbox-title');
  document.body.append(dialog);

  let at = 0;
  const show = (index) => {
    at = (index + shots.length) % shots.length;
    const shot = shots[at];
    picture.src = shot.src;
    picture.alt = shot.alt;
    title.textContent = shot.title;
    caption.textContent = shot.caption;
    count.textContent = `${at + 1} of ${shots.length}`;
    // The next picture is fetched ahead, so stepping through feels immediate.
    new Image().src = shots[(at + 1) % shots.length].src;
  };

  const root = document.documentElement;
  const open = (index) => {
    show(index);
    // The page behind must not move, and must not jump sideways when its
    // scrollbar goes away.
    const bar = window.innerWidth - root.clientWidth;
    root.style.overflow = 'hidden';
    if (bar > 0) root.style.paddingRight = `${bar}px`;
    dialog.showModal();
    close.focus();
  };
  // Put the page back as it was, with the keyboard on the picture just viewed.
  const restore = () => {
    root.style.overflow = '';
    root.style.paddingRight = '';
    shots[at].link.focus({ preventScroll: true });
  };
  const shut = () => {
    if (dialog.open) dialog.close();
    restore();
  };
  // Esc closes the dialog by itself; this hears about it.
  dialog.addEventListener('close', restore);

  links.forEach((link, index) => {
    link.addEventListener('click', (event) => {
      // Ctrl-click and the like still open the picture in a new tab.
      if (event.ctrlKey || event.metaKey || event.shiftKey || event.altKey || event.button !== 0) return;
      event.preventDefault();
      open(index);
    });
  });
  close.addEventListener('click', shut);
  previous.addEventListener('click', () => show(at - 1));
  next.addEventListener('click', () => show(at + 1));
  // A click or tap anywhere that is not the picture, the words or a button closes it.
  dialog.addEventListener('click', (event) => {
    if (event.target === dialog || event.target === stage) shut();
  });
  picture.addEventListener('click', shut);
  dialog.addEventListener('keydown', (event) => {
    if (event.key === 'ArrowLeft') { event.preventDefault(); show(at - 1); }
    if (event.key === 'ArrowRight') { event.preventDefault(); show(at + 1); }
  });
})();
