# raptorboost

A remote file/directory transfer tool with a simple interface and the following features:
- Resume support
- Content-based dedup
- Per-transfer link generation (each transfer gets its own directory that links to its content)
- Pretty progress bars

The transfer protocol is super simple: protobuf/grpc, no encryption, no authentication. It is meant to be used over a tunneled interface such as wireguard.
