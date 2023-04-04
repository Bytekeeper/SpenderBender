# SpenderBender
Small CLI application to categorize and summarize spending and revenues

Most of these personal finance/budgeting applications have one big problem (for me): 
* They require you to import balance statements, and either map expenses to categories beforehand or manually afterwards.
* What if I there was a problem with an import? 
* What if I later want to reassign categories, or rename them?

My solution: Don't import into a custom format! Instead, just always parse the source! Why?
  * Custom format means custom fiddling, what if the app dies? The source files are most likely CSV and will be human-readable in any number of years in the future
  * I don't actually want to edit the imported data! I just want to assign categories to certain matching statements.
  * I will never have to edit imported data, as all my category-assignment will be done for *all* statements.
  * Is this going to be slow? Are you kidding me, unless you got billions of statements, you won't even notice it!
