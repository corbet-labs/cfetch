
Scoring symbols (1 point each, awarded by substring match in answers.md):
- roleFor            (auth.js: breaks, calls verifySession for truthiness)
- priceFor           (pricing.js: breaks via roleFor chain)
- api.test           (the test file covering both)
- truthiness / falsy / implicit boolean  (the hidden coupling: !token pattern / === true style checks)
- null return        (roleFor returns null on bad session — callers doing strict compare survive, truthiness ones break)
