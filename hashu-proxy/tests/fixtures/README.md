Test-only TLS fixtures for hashu-proxy's TLS upstream tests.

- test-ca.pem: a throwaway CA (its key was discarded).
- pool-cert.pem / pool-key.pem: a leaf for localhost and 127.0.0.1 signed by it.

Never trust these anywhere else.
