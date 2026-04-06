# Protoset for grpcurl (no gRPC reflection on rio servers).
#
# rio-scheduler + rio-store don't register tonic-reflection. grpcurl
# without a protoset/proto can only probe the health service (via
# bundled grpc.health.v1 descriptors). For TriggerGC / SubmitBuild /
# CancelBuild we need the full service set → compile the protos to a
# FileDescriptorSet that grpcurl loads with -protoset.
#
# --include_imports: these import types.proto + google's well-known
# types (empty.proto, timestamp.proto). grpcurl needs the transitive
# closure to resolve message types. protoc bundles the well-known
# types automatically when this flag is set.
{ pkgs }:
pkgs.runCommand "rio-protoset" { buildInputs = [ pkgs.protobuf ]; } ''
  mkdir -p $out
  protoc \
    --proto_path=${../../../rio-proto/proto} \
    --descriptor_set_out=$out/rio.protoset \
    --include_imports \
    admin.proto scheduler.proto store.proto types.proto
''
