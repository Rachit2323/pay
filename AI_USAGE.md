# AI usage

I used Claude Code, a coding assistant that runs in the terminal, while building this project.

I used it to:

- Review the existing code and list the problems.
- Draft the database schema, the payment logic, the webhook worker, and the tests.
- Run the tests and the Docker setup.
- Check the API with curl.

I did not use any other AI tool.

## Decisions

1. **A timed-out payment stays `pending`.** The card may still have been charged, so I don't mark it `failed`. A retry with the same key asks the PSP again.
2. **The attempt id is the PSP idempotency key.** The attempt is saved before the PSP call, so a retry sends the same id and gets the same charge back.
3. **Row lock plus a partial unique index.** The lock makes concurrent payments wait in line, and the index blocks a second live charge. I chose this over advisory locks because the lock is visible in the query.
