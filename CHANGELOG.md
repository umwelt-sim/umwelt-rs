# Changelog

## 0.0.1

Initial release. Interest management for real-time simulation:

- Spatial subscription over a cell grid with configurable region and view radius
- Priority accumulation so nearby entities update more often than distant ones
- Per-viewer bandwidth budgeting to an MTU-sized packet
- Region simulation server over NATS, edge relay over QUIC
- Fixed-point position arithmetic (1024 units per meter)
