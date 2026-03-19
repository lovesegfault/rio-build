<script lang="ts">
  // Compile-time proof that TS proto codegen produces types that svelte-check
  // accepts and vite can bundle. AdminService is a GenService<...> descriptor
  // (protobuf-es v2 unified codegen — not the v1 *_connect.ts layout).
  // Referencing .typeName forces a value-position use so the import isn't
  // tree-shaken away before the build check runs; satisfies ensures the shape
  // matches what @connectrpc/connect's createClient() expects without pulling
  // in a transport yet.
  import { AdminService } from './gen/admin_pb';
  import type { DescService } from '@bufbuild/protobuf';

  const svc = AdminService satisfies DescService;
</script>

<h1>rio-dashboard</h1>
<p>admin: {svc.typeName}</p>
