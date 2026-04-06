<script lang="ts">
  // App shell: sidebar nav + route outlet. svelte-routing is the minimal
  // history-API router that works with Svelte's SSR-aware compile (the
  // library is Svelte-4-syntax, which Svelte 5 compiles in legacy-interop
  // mode — no runes inside the library, runes in our pages are fine).
  //
  // ONE build artifact: basepath left at default ('/'); deployment under
  // a subpath would need a VITE_BASE env, which we don't have or want
  // (prod nginx serves the SPA at root; see dashboard helm templates).
  import { Link, Route, Router } from 'svelte-routing';
  import Builds from './pages/Builds.svelte';
  import Cluster from './pages/Cluster.svelte';
  import Workers from './pages/Workers.svelte';
</script>

<Router>
  <nav>
    <h1>rio-dashboard</h1>
    <ul>
      <li><Link to="/">Cluster</Link></li>
      <li><Link to="/builds">Builds</Link></li>
      <li><Link to="/workers">Workers</Link></li>
    </ul>
  </nav>
  <main>
    <Route path="/"><Cluster /></Route>
    <Route path="/builds" component={Builds} />
    <Route path="/builds/:id" component={Builds} />
    <Route path="/workers" component={Workers} />
  </main>
</Router>
