# Self-Evolution System

Autonomous improvement without LLM weight training — evolves prompts, memory, and strategies based on interaction data.

## Pipeline

```
Record (realtime) → Analyze (daily) → Evolve (every 3 days)
```

## Components

- **Record layer**: Trace every interaction, tool call, and outcome
- **Memory system**: Retrieval, safety filtering, compression, anti-pattern detection
- **Analysis**: Automated evaluation with judge model and test suites
- **Evolution engines**: Memory evolution, prompt evolution, strategy evolution
- **Safety**: Rollback capability, gate checks, shadow mode for first rounds
- **Pipeline**: Scheduler, pipeline orchestration, annotation system
- **22 modules, ~9500 lines** of evolution infrastructure

## CLI

```bash
openprx evolution status    # Show evolution state
openprx evolution trigger   # Manually trigger evolution cycle
openprx evolution rollback  # Rollback last evolution
```
