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
- **Integrated runtime**: evolution state, history, configuration, and manual
  trigger commands are exposed through the main CLI

## CLI

```bash
prx evolution status   # Show evolution state
prx evolution history  # Show evolution history
prx evolution trigger  # Manually trigger one evolution cycle
```
