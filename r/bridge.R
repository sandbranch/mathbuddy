# mathbuddy bridge: lets mathbuddy, running in a terminal (RStudio's Terminal
# tab, tmux, anything), see and help in this R session.
#
# Load it once per session, e.g. from ~/.Rprofile:
#   if (interactive()) try(source("<path to this file>"), silent = TRUE)
# (`mathbuddy setup-r` adds that line for you.)
#
# What it does:
# * records each top-level command you run, a short printout of its visible
#   result, and errors, in a small ring buffer;
# * serves them, plus your variable list and read-only inspection, on
#   127.0.0.1 at a random port, answering only requests that carry a random
#   token. Port and token go in a file only you can read, under
#   $XDG_RUNTIME_DIR/mathbuddy/, deleted when R exits;
# * runs code mathbuddy sends only after you approved it there. Plots go to
#   your active device (in RStudio, the Plots pane).
# Requests are answered while R is idle at the prompt; while your own code
# runs, mathbuddy waits.
#
# Nothing is added to your global environment.

local({
  if (!interactive()) return(invisible(NULL))
  if (!is.null(getOption("mathbuddy.bridge"))) return(invisible(NULL))  # already running
  for (p in c("httpuv", "later", "jsonlite")) {
    if (!requireNamespace(p, quietly = TRUE)) {
      message("mathbuddy bridge: needs the '", p, "' package; install.packages('", p, "')")
      return(invisible(NULL))
    }
  }

  st <- new.env(parent = emptyenv())
  st$events <- list()
  st$seq <- 0L
  st$in_eval <- FALSE
  st$started <- as.numeric(Sys.time())
  st$token <- paste(sample(c(letters, LETTERS, 0:9), 40, replace = TRUE), collapse = "")

  # Count new plot pages (base graphics and grid/ggplot2 both have a hook), so
  # mathbuddy can tell a plot was drawn instead of "no output".
  st$plots <- 0L
  st$plots_seen <- 0L
  st$plot_device <- ""
  st$plot_hook <- function() {
    st$plots <- st$plots + 1L
    st$plot_device <- tryCatch(names(grDevices::dev.cur()), error = function(e) "")  # where it went, before any dev.off()
  }
  setHook("plot.new", st$plot_hook, "append")
  setHook("grid.newpage", st$plot_hook, "append")

  max_events <- 60L
  clip <- function(s, n = 2000L) {
    s <- paste(s, collapse = "\n")
    if (nchar(s) > n) paste0(substr(s, 1L, n %/% 2L), "\n...(truncated)...\n", substr(s, nchar(s) - n %/% 2L, nchar(s))) else s
  }
  add_event <- function(code, value = "", error = NULL) {
    st$seq <- st$seq + 1L
    ev <- list(seq = st$seq, t = as.numeric(Sys.time()), code = clip(code, 3000L), value = clip(value), error = error)
    st$events <- c(utils::tail(st$events, max_events - 1L), list(ev))
  }

  # Printing a result again must not redraw a plot or open a viewer, so only
  # plain data-like values are printed; everything else is summarised.
  printable <- function(v) {
    is.atomic(v) || is.data.frame(v) || is.matrix(v) || is.factor(v) ||
      inherits(v, c("table", "htest", "lm", "glm", "summary.lm", "summary.glm", "anova", "summaryDefault", "difftime"))
  }
  show_value <- function(v) {
    if (!printable(v)) return(sprintf("<%s>", paste(class(v), collapse = "/")))
    if (utils::object.size(v) > 2e5) return(sprintf("<%s, %s>", class(v)[1], format(utils::object.size(v), units = "auto")))
    tryCatch(utils::capture.output(print(v)), error = function(e) "")
  }
  deparse_code <- function(expr) paste(deparse(expr, width.cutoff = 200L), collapse = "\n")

  task_cb <- function(expr, value, ok, visible) {
    drew <- st$plots - st$plots_seen
    st$plots_seen <- st$plots
    if (!st$in_eval) {
      shown <- if (isTRUE(ok) && isTRUE(visible)) show_value(value) else ""
      if (drew > 0L) shown <- c(shown[nzchar(shown)], sprintf("(drew %d plot%s)", drew, if (drew > 1L) "s" else ""))
      add_event(deparse_code(expr), shown)
    }
    TRUE
  }
  st$task_id <- addTaskCallback(task_cb, name = "mathbuddy")

  # A failing command never reaches the task callback, so errors come from a
  # global calling handler. It only sees errors nothing else caught.
  on_error <- function(e) {
    if (st$in_eval) return(invisible(NULL))
    call <- conditionCall(e)
    # The handler's own frame is last; the first frame (if any) is the top-level command.
    calls <- utils::head(sys.calls(), -1L)
    internal <- function(cl) is.name(cl[[1L]]) && as.character(cl[[1L]]) %in% c(".handleSimpleError", "h", "stop")
    calls <- Filter(Negate(internal), calls)
    top <- if (length(calls)) calls[[1L]] else NULL
    code <- if (!is.null(top)) deparse_code(top) else if (!is.null(call)) deparse_code(call) else "(unknown command)"
    msg <- conditionMessage(e)
    where <- if (!is.null(call)) paste0("Error in ", deparse_code(call), ": ") else "Error: "
    add_event(code, "", paste0(where, msg))
  }
  # globalCallingHandlers() refuses to run while any handler is on the stack,
  # which is the case inside source() and inside any tryCatch guard. A `later`
  # callback runs from the idle event loop with an empty stack, so register
  # there. (httpuv needs `later` anyway.)
  st$errors <- FALSE
  later::later(function() {
    globalCallingHandlers(error = on_error)
    st$errors <- TRUE
  }, 0)

  # -- what mathbuddy may ask for -----------------------------------------------

  describe_var <- function(n, v) {
    d <- dim(v)
    extra <- if (!is.null(d)) paste0(" dim=", paste(d, collapse = "x"))
             else if (is.atomic(v) || is.list(v)) paste0(" length=", length(v)) else ""
    if (is.data.frame(v)) extra <- paste0(extra, " cols=", clip(paste(names(v), collapse = ","), 200L))
    paste0(n, ": ", paste(class(v), collapse = "/"), extra)
  }
  list_vars <- function() {
    nms <- ls(globalenv())
    if (!length(nms)) return("(no variables in the global environment)")
    paste(vapply(nms, function(n) describe_var(n, get(n, envir = globalenv())), ""), collapse = "\n")
  }

  # Only names, $, [[ ]] and [ ] with constants: looking must not change anything.
  safe_expr <- function(e) {
    if (is.name(e) || (is.atomic(e) && length(e) == 1L)) return(TRUE)
    if (is.call(e)) {
      f <- e[[1L]]
      if (is.name(f) && as.character(f) %in% c("$", "[[", "[", "@")) {
        return(all(vapply(as.list(e)[-1L], function(a) identical(a, quote(expr = )) || safe_expr(a), TRUE)))
      }
      if (is.name(f) && as.character(f) == ":" && all(vapply(as.list(e)[-1L], function(a) is.atomic(a), TRUE))) return(TRUE)
    }
    FALSE
  }
  inspect <- function(txt) {
    e <- tryCatch(parse(text = txt, keep.source = FALSE), error = function(err) NULL)
    if (is.null(e) || length(e) != 1L || !safe_expr(e[[1L]])) {
      return("refused: only names, $, [[ ]] and [ ] with constants can be inspected (no function calls)")
    }
    v <- tryCatch(eval(e[[1L]], globalenv()), error = function(err) structure(conditionMessage(err), class = "mb_err"))
    if (inherits(v, "mb_err")) return(paste("Error:", unclass(v)))
    out <- c(paste0(txt, ": ", paste(class(v), collapse = "/")))
    if (is.function(v)) {
      out <- c(out, utils::capture.output(print(args(v))))
    } else {
      out <- c(out, utils::capture.output(utils::str(v, max.level = 1L, list.len = 25L, vec.len = 4L)))
      if (is.data.frame(v)) {
        out <- c(out, "head:", utils::capture.output(print(utils::head(v, 8L))))
        out <- c(out, "summary:", utils::capture.output(print(summary(v))))
      } else if (is.numeric(v) && length(v) > 1L) {
        out <- c(out, "summary:", utils::capture.output(print(summary(v))))
      }
    }
    clip(out, 2500L)
  }

  run_code <- function(code) {
    st$in_eval <- TRUE
    on.exit(st$in_eval <- FALSE)
    plots_before <- st$plots
    notes <- character()
    status <- "ok"
    out <- utils::capture.output({
      withCallingHandlers(
        tryCatch({
          for (e in parse(text = code, keep.source = FALSE)) {
            v <- withVisible(eval(e, globalenv()))
            if (v$visible) print(v$value)  # autoprint, as the console would: this is what draws a ggplot
          }
        }, error = function(err) {
          status <<- "error"
          cat("Error: ", conditionMessage(err), "\n", sep = "")
        }),
        warning = function(w) { notes <<- c(notes, paste("Warning:", conditionMessage(w))); invokeRestart("muffleWarning") },
        message = function(m) { notes <<- c(notes, sub("\n$", "", conditionMessage(m))); invokeRestart("muffleMessage") }
      )
    }, type = "output")
    drew <- st$plots - plots_before
    st$plots_seen <- st$plots
    list(status = status, output = clip(c(out, notes), 4000L), plots = drew,
         device = if (drew > 0L) st$plot_device else "")
  }

  rstudio <- function() requireNamespace("rstudioapi", quietly = TRUE) && isTRUE(rstudioapi::isAvailable())
  frontend <- if (identical(Sys.getenv("RSTUDIO"), "1")) "RStudio"
              else if (nzchar(Sys.getenv("POSITRON"))) "Positron" else "R console"

  info <- function() {
    label <- basename(getwd())
    doc <- ""
    if (rstudio()) {
      proj <- tryCatch(rstudioapi::getActiveProject(), error = function(e) NULL)
      if (!is.null(proj)) label <- basename(proj)
      doc <- tryCatch(rstudioapi::getSourceEditorContext()$path, error = function(e) "")
    }
    list(pid = Sys.getpid(), r_version = paste(R.version$major, R.version$minor, sep = "."),
         frontend = frontend, cwd = getwd(), label = label, document = if (is.null(doc)) "" else doc,
         started = st$started, last_event = if (length(st$events)) st$events[[length(st$events)]]$t else NULL,
         n_vars = length(ls(globalenv())), errors_tracked = st$errors, rstudio_api = rstudio(),
         graphics = tryCatch(names(grDevices::dev.cur()), error = function(e) ""),
         packages = Filter(function(p) requireNamespace(p, quietly = TRUE), c("ggplot2", "dplyr", "tidyr", "data.table")))
  }

  insert_code <- function(code) {
    if (!rstudio()) return(list(status = "error", output = "not running in RStudio"))
    ctx <- rstudioapi::getSourceEditorContext()
    if (is.null(ctx)) return(list(status = "error", output = "no script is open in the editor"))
    rng <- ctx$selection[[1L]]$range
    rstudioapi::insertText(location = rng, text = paste0(sub("\n*$", "", code), "\n"), id = ctx$id)
    list(status = "ok", output = paste("inserted into", if (nzchar(ctx$path)) basename(ctx$path) else "the open script"))
  }
  to_console <- function(code) {
    if (!rstudio()) return(list(status = "error", output = "not running in RStudio"))
    rstudioapi::sendToConsole(code, execute = TRUE, focus = FALSE)
    list(status = "ok", output = "sent to the console")
  }

  # -- HTTP -------------------------------------------------------------------

  reply <- function(obj, status = 200L) {
    list(status = status, headers = list("Content-Type" = "application/json"),
         body = as.character(jsonlite::toJSON(obj, auto_unbox = TRUE, null = "null", force = TRUE)))
  }
  handle <- function(req) {
    if (!identical(req$HTTP_X_MATHBUDDY_TOKEN, st$token)) return(reply(list(error = "bad token"), 403L))
    body <- tryCatch({
      raw <- req$rook.input$read()
      if (length(raw)) jsonlite::fromJSON(rawToChar(raw), simplifyVector = TRUE) else list()
    }, error = function(e) list())
    path <- req$PATH_INFO
    tryCatch(switch(path,
      "/info" = reply(info()),
      "/events" = {
        since <- suppressWarnings(as.integer(sub(".*since=([0-9]+).*", "\\1", req$QUERY_STRING)))
        if (is.na(since)) since <- 0L
        reply(list(events = Filter(function(e) e$seq > since, st$events), seq = st$seq))
      },
      "/vars" = reply(list(text = list_vars())),
      "/inspect" = reply(list(text = inspect(as.character(body$expr)))),
      "/run" = reply(run_code(as.character(body$code))),
      "/insert" = reply(insert_code(as.character(body$code))),
      "/console" = reply(to_console(as.character(body$code))),
      reply(list(error = paste("no such endpoint", path)), 404L)
    ), error = function(e) reply(list(error = conditionMessage(e)), 500L))
  }

  port <- httpuv::randomPort(min = 20000L, max = 60000L, host = "127.0.0.1")
  st$server <- httpuv::startServer("127.0.0.1", port, list(call = handle))

  dir <- Sys.getenv("XDG_RUNTIME_DIR")
  if (!nzchar(dir) || !dir.exists(dir)) dir <- tempdir()
  dir <- file.path(dir, "mathbuddy")
  dir.create(dir, showWarnings = FALSE, recursive = TRUE, mode = "0700")
  st$file <- file.path(dir, sprintf("r-%d.json", Sys.getpid()))
  old <- Sys.umask("077")
  writeLines(as.character(jsonlite::toJSON(list(
    kind = "r-bridge", pid = Sys.getpid(), port = port, token = st$token, frontend = frontend,
    cwd = getwd(), started = st$started, version = 1L), auto_unbox = TRUE)), st$file)
  Sys.umask(old)
  Sys.chmod(st$file, "0600")

  st$stop <- function() {
    try(httpuv::stopServer(st$server), silent = TRUE)
    try(removeTaskCallback("mathbuddy"), silent = TRUE)
    for (h in c("plot.new", "grid.newpage")) {
      setHook(h, Filter(function(f) !identical(f, st$plot_hook), getHook(h)), "replace")
    }
    unlink(st$file)
    options(mathbuddy.bridge = NULL)
  }
  reg.finalizer(st, function(e) unlink(e$file), onexit = TRUE)
  options(mathbuddy.bridge = st)
  invisible(NULL)
})
