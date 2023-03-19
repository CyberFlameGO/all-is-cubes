// This is the "root" _module_ which is loaded by bootstrap_nonmod.js and imports all others.

import init, { start_game } from './all_is_cubes_wasm.js';


document.getElementById('loading-log').innerText = 'Loading code...';

// init() is a function generated by wasm-pack (wasm-bindgen?) which loads the actual wasm
init().then(() => start_game()).catch(error => {
  // TODO: This no longer does very much useful since start_game() is async and the panic
  // doesn't turn into a promise failure.

  document.getElementById('loading-log').innerText +=
    '\nError during initial loading! Check console for details.';

  if (String(error) !== 'RuntimeError: unreachable') {
    // Only log errors that aren't Rust's panics because those are logged separately.
    console.error(error);
  }
});
