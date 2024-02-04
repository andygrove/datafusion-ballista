# PyBallista

Minimal Python client for Ballista.

The goal of this project is to provide a way to run SQL against a Ballista cluster from Python and collect results.

The goal is not to provide the full DataFrame API. This could be added later if there is sufficient interest from maintainers.

This project is versioned and released independently from the main Ballista project.

```python
from pyballista import SessionContext

# Connect to Ballista scheduler
ctx = SessionContext("localhost", 50050)

# Execute query
results = ctx.sql("SELECT 1")
```