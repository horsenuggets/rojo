return function()
	local ApiContext = require(script.Parent.ApiContext)
	local Http = require(script.Parent.Parent.Packages.Http)
	local Promise = require(script.Parent.Parent.Packages.Promise)

	local function startupResponses()
		local Config = require(script.Parent.Config)
		return {
			info = {
				sessionId = "session",
				serverVersion = "7.7.1",
				protocolVersion = Config.protocolVersion,
				projectName = "Project",
				rootInstanceId = "ROOT",
			},
			read = { sessionId = "session", messageCursor = 4, instances = {} },
		}
	end

	-- Every timing boundary is controlled here, including an HTTP response arriving late.
	local function withStartupHttp(run)
		local originalGet, originalDelay = Http.get, Promise.delay
		local control = { requests = {}, delays = {}, responses = startupResponses() }
		local function pending(list, fields)
			local promise = Promise.new(function(resolve, reject, onCancel)
				fields.resolve, fields.reject = resolve, reject
				onCancel(function()
					fields.cancelled = true
				end)
			end)
			fields.promise = promise
			table.insert(list, fields)
			return promise
		end
		Http.get = function(url)
			return pending(control.requests, { url = url })
		end
		Promise.delay = function(seconds)
			expect(seconds > 0 and seconds <= 1).to.equal(true)
			return pending(control.delays, { seconds = seconds })
		end
		control.answer = function(index, phase, mutate)
			local body = table.clone(control.responses[phase])
			if mutate then
				mutate(body)
			end
			control.requests[index].resolve({ code = 200, body = Http.msgpackEncode(body) })
		end
		local success, problem = pcall(run, control)
		Http.get, Promise.delay = originalGet, originalDelay
		assert(success, problem)
	end

	it("rejects malformed directional info with optional typechecking disabled", function()
		local Settings = require(script.Parent.Settings)
		local originalGet = Settings.get
		Settings.get = function(_, key)
			return if key == "typecheckingEnabled" then false else originalGet(Settings, key)
		end
		local success, problem = pcall(function()
			withStartupHttp(function(control)
				local context = ApiContext.new("http://localhost")
				local connection = context:connect()
				control.answer(1, "info", function(body)
					body.syncDirection = "invalid"
					body.protocolVersion = 13
				end)
				expect(connection:await()).to.equal(false)
				expect(context.__sessionId).to.equal(nil)
				expect(context:write({}, nil, "Studio"):await()).to.equal(false)
			end)
		end)
		Settings.get = originalGet
		assert(success, problem)
	end)

	it("blocks direct API writes and modifying recovery endpoints under filesystem-only policy", function()
		withStartupHttp(function(control)
			local context = ApiContext.new("http://localhost")
			local connection = context:connect()
			control.answer(1, "info", function(body)
				body.syncDirection = "fs-to-studio"
				body.protocolVersion = 13
				body.supportedSyncDirections = { "fs-to-studio", "studio-to-fs", "bidirectional" }
			end)
			expect(connection:await()).to.equal(true)
			local originalPost, originalDelete = Http.post, Http.delete
			local sent = 0
			Http.post = function()
				sent += 1
				return Promise.reject("Unexpected write.")
			end
			Http.delete = Http.post
			local success, problem = pcall(function()
				expect(context:write({}, nil, "Studio"):await()).to.equal(false)
				expect(context:createSourceCheckpoint("SCRIPT", "draft", "disk", 0):await()).to.equal(false)
				expect(context:discardSourceCheckpoint("checkpoint"):await()).to.equal(false)
				expect(sent).to.equal(0)
			end)
			Http.post, Http.delete = originalPost, originalDelete
			assert(success, problem)
		end)
	end)

	it("closes invalid provenance packets instead of leaving a writable socket after validation fails", function()
		local Settings = require(script.Parent.Settings)
		local originalGet = Settings.get
		Settings.get = function(_, key)
			return if key == "typecheckingEnabled" then true else originalGet(Settings, key)
		end
		local success, problem = pcall(function()
			local function signal()
				local listeners = {}
				return {
					Connect = function(_, callback)
						local connection = {}
						listeners[connection] = callback
						function connection:Disconnect()
							listeners[self] = nil
						end
						return connection
					end,
					Fire = function(_, value)
						for _, callback in table.clone(listeners) do
							callback(value)
						end
					end,
				}
			end
			local context = ApiContext.new("http://localhost")
			context.__sessionId = "session"
			local closed, handled = 0, false
			local client = { MessageReceived = signal(), Closed = signal(), Error = signal() }
			client.Close = function()
				closed += 1
				client.Closed:Fire()
			end
			context.__createWebStreamClient = function()
				return client
			end
			local socket = context:connectWebSocket({
				messages = function()
					handled = true
				end,
			})
			client.MessageReceived:Fire(Http.msgpackEncode({
				sessionId = "session",
				packetType = "messages",
				body = {
					messageCursor = 1,
					messages = { { origin = "ambiguous", removed = {}, added = {}, updated = {} } },
				},
			}))
			expect(socket:await()).to.equal(false)
			expect(context.__connected).to.equal(false)
			expect(handled).to.equal(false)
			expect(closed).to.equal(1)
		end)
		Settings.get = originalGet
		assert(success, problem)
	end)

	-- Replace only the clock and scheduler boundaries of the real yield budget.
	local function withDecodeClock(run)
		local yieldBudget = require(script.Parent.yieldBudget)
		local environment = getfenv(yieldBudget)
		local control = { threads = {}, time = 0 }
		local clock = table.clone(os)
		clock.clock = function()
			control.time += 1
			return control.time
		end
		local budgetEnvironment = setmetatable({}, { __index = environment })
		budgetEnvironment.os = clock
		budgetEnvironment.task = {
			wait = function()
				table.insert(control.threads, coroutine.running())
				coroutine.yield()
			end,
		}
		setfenv(yieldBudget, budgetEnvironment)
		local success, problem = pcall(run, control)
		setfenv(yieldBudget, environment)
		assert(success, problem)
	end

	for _, retirement in { "disconnect", "supersede" } do
		for _, boundary in { "before", "after" } do
			it(`should abort a {retirement} read {boundary} yielding without parsing further or retrying`, function()
				withStartupHttp(function(control)
					withDecodeClock(function(clock)
						local context = ApiContext.new("http://localhost")
						context.__sessionId = "session"
						local function retire()
							if retirement == "disconnect" then
								context:disconnect()
							else
								context:connect()
								control.answer(2, "info", function(body)
									body.sessionId = "new"
								end)
							end
						end
						local original = Http.Response.msgpack
						local completed, decodeError, checkpoints = false, nil, 0
						local success, problem = pcall(function()
							Http.Response.msgpack = function(response, checkpoint)
								if checkpoint == nil then
									return original(response)
								end
								local ok, body = pcall(original, response, function()
									checkpoints += 1
									if boundary == "before" then
										retire()
									end
									checkpoint()
								end)
								if not ok then
									decodeError = body
									error(body)
								end
								completed = true
								return body
							end
							local delivered = false
							local read = context:read({ "ROOT" }):andThen(function()
								delivered = true
							end)
							-- A malformed token beyond the first checkpoint must never be reached.
							control.requests[1].resolve({
								code = 200,
								body = "\xDD\x00\x00\x10\x01" .. string.rep("\x00", 4096) .. "\xC1",
							})
							expect(checkpoints).to.equal(1)
							expect(completed).to.equal(false)
							expect(delivered).to.equal(false)
							expect(#clock.threads).to.equal(if boundary == "before" then 0 else 1)
							if boundary == "after" then
								retire()
								expect(coroutine.resume(clock.threads[1])).to.equal(true)
							end
							expect(tostring(decodeError):find("The connection is closed.", 1, true)).to.be.ok()
							expect(getmetatable(decodeError) == Http.Error).to.equal(false)
							expect(checkpoints).to.equal(1)
							expect(completed).to.equal(false)
							expect(delivered).to.equal(false)
							expect(read:getStatus() == Promise.Status.Resolved).to.equal(false)
							expect(context.__sessionId).to.equal(if retirement == "supersede" then "new" else "session")
							expect(#control.requests).to.equal(if retirement == "supersede" then 2 else 1)
							expect(#control.delays).to.equal(0)
						end)
						Http.Response.msgpack = original
						assert(success, problem)
					end)
				end)
			end)
		end
	end

	it("should stop a suspended decode when only its returned read is cancelled", function()
		withStartupHttp(function(control)
			withDecodeClock(function(clock)
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "session"
				local delivered = false
				local read = context:read({ "ROOT" })
				read:andThen(function()
					delivered = true
				end)
				control.requests[1].resolve({
					code = 200,
					body = "\xDD\x00\x00\x10\x01" .. string.rep("\x00", 4096) .. "\xC1",
				})
				expect(#clock.threads).to.equal(1)
				local generation = context.__generation
				read:cancel()
				expect(read:getStatus()).to.equal(Promise.Status.Cancelled)
				expect(coroutine.resume(clock.threads[1])).to.equal(true)
				expect(#clock.threads).to.equal(1)
				expect(delivered).to.equal(false)
				expect(context.__connected).to.equal(true)
				expect(context.__generation).to.equal(generation)
				expect(context.__sessionId).to.equal("session")
				expect(#control.requests).to.equal(1)
				expect(#control.delays).to.equal(0)
			end)
		end)
	end)

	for _, cancelFirst in { false, true } do
		for _, cancelSecond in { false, true } do
			it(
				`should retain decoder ownership for active consumers with cancellations {cancelFirst}/{cancelSecond}`,
				function()
					withStartupHttp(function(control)
						withDecodeClock(function(clock)
							local context = ApiContext.new("http://localhost")
							context.__sessionId = "session"
							local delivered = { false, false }
							local read = context:read({ "ROOT" })
							local consumers = {}
							for index = 1, 2 do
								consumers[index] = read:andThen(function(body)
									delivered[index] = true
									return body
								end)
							end
							control.answer(1, "read", function(body)
								body.padding = table.create(2500, 0)
							end)
							expect(#clock.threads).to.equal(1)
							if cancelFirst then
								consumers[1]:cancel()
							end
							if cancelSecond then
								consumers[2]:cancel()
							end
							expect(coroutine.resume(clock.threads[1])).to.equal(true)
							if cancelFirst and cancelSecond then
								expect(read:getStatus()).to.equal(Promise.Status.Cancelled)
								expect(#clock.threads).to.equal(1)
							else
								expect(#clock.threads).to.equal(2)
								expect(coroutine.resume(clock.threads[2])).to.equal(true)
								expect(read:getStatus()).to.equal(Promise.Status.Resolved)
							end
							expect(delivered[1]).to.equal(not cancelFirst)
							expect(delivered[2]).to.equal(not cancelSecond)
							expect(context.__connected).to.equal(true)
							expect(#control.requests).to.equal(1)
							expect(#control.delays).to.equal(0)
						end)
					end)
				end
			)
		end
	end

	it("should resume a healthy read only after its decode checkpoints are released", function()
		withStartupHttp(function(control)
			withDecodeClock(function(clock)
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "session"
				local delivered = false
				local read = context:read({ "ROOT" }):andThen(function(body)
					delivered = true
					return body
				end)
				control.answer(1, "read", function(body)
					body.padding = table.create(2500, 0)
				end)
				expect(#clock.threads).to.equal(1)
				expect(delivered).to.equal(false)
				expect(coroutine.resume(clock.threads[1])).to.equal(true)
				expect(#clock.threads).to.equal(2)
				expect(delivered).to.equal(false)
				expect(coroutine.resume(clock.threads[2])).to.equal(true)
				local accepted, body = read:await()
				expect(accepted).to.equal(true)
				expect(delivered).to.equal(true)
				expect(#body.padding).to.equal(2500)
				expect(#control.requests).to.equal(1)
				expect(#control.delays).to.equal(0)
			end)
		end)
	end)

	for _, phase in { "info", "read" } do
		local function request(context)
			if phase == "info" then
				return context:connect()
			end
			context.__sessionId = "session"
			return context:read({ "ROOT" })
		end

		it(`should retry startup {phase} timeout and connection failures then succeed`, function()
			withStartupHttp(function(control)
				for _, kind in { Http.Error.Kind.Timeout, Http.Error.Kind.ConnectFailed } do
					local context = ApiContext.new("http://localhost")
					local index, delayIndex = #control.requests + 1, #control.delays + 1
					local result = request(context)
					control.requests[index].reject(Http.Error.new(kind))
					expect(#control.requests).to.equal(index)
					expect(#control.delays).to.equal(delayIndex)
					control.delays[delayIndex].resolve()
					control.answer(index + 1, phase)
					local accepted, body = result:await()
					expect(accepted).to.equal(true)
					expect(body.sessionId).to.equal("session")
					expect(context.__sessionId).to.equal("session")
					expect(context:getMessageCursor()).to.equal(-1)
					expect(next(context.__activeRequests)).to.equal(nil)
				end
			end)
		end)

		it(`should bound startup {phase} exhaustion and retain typed phase diagnostics`, function()
			withStartupHttp(function(control)
				local context = ApiContext.new("http://localhost")
				local result = request(context)
				for attempt = 1, 3 do
					control.requests[attempt].reject(Http.Error.new(Http.Error.Kind.Timeout))
					if attempt < 3 then
						control.delays[attempt].resolve()
					end
				end
				local accepted, problem = result:await()
				expect(accepted).to.equal(false)
				expect(problem.type).to.equal(Http.Error.Kind.Timeout)
				expect(tostring(problem):find(if phase == "info" then "/api/rojo" else "/api/read", 1, true)).to.be.ok()
				expect(tostring(problem):find("3 attempts", 1, true)).to.be.ok()
				expect(#control.requests).to.equal(3)
				expect(#control.delays).to.equal(2)
				expect(next(context.__activeRequests)).to.equal(nil)
			end)
		end)

		it(`should not retry permanent startup {phase} transport errors or timeout strings`, function()
			withStartupHttp(function(control)
				for _, problem in
					{
						Http.Error.new(Http.Error.Kind.HttpNotEnabled),
						Http.Error.new(Http.Error.Kind.Protocol, "Invalid protocol."),
						Http.Error.new(Http.Error.Kind.Unknown, "Unknown failure."),
						"HttpError: Timedout",
						{ type = Http.Error.Kind.Timeout },
					}
				do
					local context = ApiContext.new("http://localhost")
					local index = #control.requests + 1
					local result = request(context)
					control.requests[index].reject(problem)
					expect(result:await()).to.equal(false)
					expect(#control.requests).to.equal(index)
					expect(#control.delays).to.equal(0)
					expect(next(context.__activeRequests)).to.equal(nil)
				end
			end)
		end)

		it(`should cancel pending startup {phase} HTTP and ignore its late response`, function()
			withStartupHttp(function(control)
				local context = ApiContext.new("http://localhost")
				local continued = false
				local result = request(context):andThen(function()
					continued = true
				end)
				expect(next(context.__activeRequests) ~= nil).to.equal(true)
				context:disconnect()
				control.answer(1, phase)
				expect(control.requests[1].cancelled).to.equal(true)
				expect(result:getStatus()).to.equal(Promise.Status.Cancelled)
				expect(continued).to.equal(false)
				expect(context.__sessionId).to.equal(if phase == "info" then nil else "session")
				expect(request(context):await()).to.equal(false)
				expect(#control.requests).to.equal(1)
				expect(#control.delays).to.equal(0)
				expect(next(context.__activeRequests)).to.equal(nil)
			end)
		end)

		it(`should cancel startup {phase} backoff without dispatching another GET`, function()
			withStartupHttp(function(control)
				local context = ApiContext.new("http://localhost")
				local result = request(context)
				control.requests[1].reject(Http.Error.new(Http.Error.Kind.Timeout))
				expect(#control.delays).to.equal(1)
				context:disconnect()
				control.delays[1].resolve()
				expect(control.delays[1].cancelled).to.equal(true)
				expect(result:getStatus()).to.equal(Promise.Status.Cancelled)
				expect(#control.requests).to.equal(1)
				expect(next(context.__activeRequests)).to.equal(nil)
			end)
		end)
	end

	it("should not retry startup HTTP status, decode, protocol, place, session or validation failures", function()
		local Settings = require(script.Parent.Settings)
		local enabled = Settings:get("typecheckingEnabled")
		local success, problem = pcall(function()
			Settings:set("typecheckingEnabled", true)
			withStartupHttp(function(control)
				for _, phase in { "info", "read" } do
					for _, kind in { "status", "decode", "validation", "identity" } do
						local context = ApiContext.new("http://localhost")
						context.__sessionId = "session"
						local index = #control.requests + 1
						local result = if phase == "info" then context:connect() else context:read({ "ROOT" })
						if kind == "status" then
							control.requests[index].resolve({ code = 503, body = "private response body" })
						elseif kind == "decode" then
							control.requests[index].resolve({ code = 200, body = string.char(0x81) })
						else
							control.answer(index, phase, function(body)
								if kind == "validation" then
									if phase == "info" then
										body.rootInstanceId = false
									else
										body.instances = false
									end
								elseif phase == "info" then
									body.protocolVersion = -1
								else
									body.sessionId = "other"
								end
							end)
						end
						local accepted, message = result:await()
						expect(accepted).to.equal(false)
						if kind == "status" then
							expect(tostring(message):find("private response body", 1, true)).to.equal(nil)
						end
						expect(#control.requests).to.equal(index)
						expect(#control.delays).to.equal(0)
						expect(next(context.__activeRequests)).to.equal(nil)
					end
				end
				for _, field in { "expectedPlaceIds", "unexpectedPlaceIds" } do
					local index = #control.requests + 1
					local result = ApiContext.new("http://localhost"):connect()
					control.answer(index, "info", function(body)
						body[field] = { if field == "expectedPlaceIds" then game.PlaceId + 1 else game.PlaceId }
					end)
					expect(result:await()).to.equal(false)
					expect(#control.requests).to.equal(index)
					expect(#control.delays).to.equal(0)
				end
			end)
		end)
		Settings:set("typecheckingEnabled", enabled)
		assert(success, problem)
	end)

	it("should supersede pending connect and read continuations without changing the new session", function()
		withStartupHttp(function(control)
			local context = ApiContext.new("http://localhost")
			local first = context:connect()
			local second = context:connect()
			control.answer(2, "info")
			control.answer(1, "info", function(body)
				body.sessionId = "stale"
			end)
			expect(second:await()).to.equal(true)
			expect(first:getStatus()).to.equal(Promise.Status.Cancelled)
			local read = context:read({ "ROOT" })
			local third = context:connect()
			control.answer(4, "info", function(body)
				body.sessionId = "new"
			end)
			control.answer(3, "read")
			expect(third:await()).to.equal(true)
			expect(read:getStatus()).to.equal(Promise.Status.Cancelled)
			expect(context.__sessionId).to.equal("new")
			expect(next(context.__activeRequests)).to.equal(nil)
		end)
	end)

	it("should clean up healthy GETs without delaying or changing publication cursors", function()
		withStartupHttp(function(control)
			local context = ApiContext.new("http://localhost")
			context:setMessageCursor(10)
			local info = context:connect()
			control.answer(1, "info")
			expect(info:await()).to.equal(true)
			local read = context:read({ "ROOT" })
			control.answer(2, "read")
			expect(read:await()).to.equal(true)
			expect(context:getMessageCursor()).to.equal(10)
			expect(#control.requests).to.equal(2)
			expect(#control.delays).to.equal(0)
			expect(next(context.__activeRequests)).to.equal(nil)
		end)
	end)

	it("should cancel the GET or backoff when its caller cancels", function()
		withStartupHttp(function(control)
			for _, backoff in { false, true } do
				local context = ApiContext.new("http://localhost")
				local index = #control.requests + 1
				local result = context:connect()
				local boundary = control.requests[index]
				if backoff then
					boundary.reject(Http.Error.new(Http.Error.Kind.ConnectFailed))
					boundary = control.delays[#control.delays]
				end
				result:cancel()
				expect(boundary.cancelled).to.equal(true)
				boundary.resolve()
				expect(#control.requests).to.equal(index)
				expect(next(context.__activeRequests)).to.equal(nil)
			end
		end)
	end)

	it("should retain HTTP error kinds without exposing endpoint credentials or response bodies", function()
		withStartupHttp(function(control)
			local context = ApiContext.new("http://private:secret@localhost")
			local result = context:read({ "private-id" })
			local failure = Http.Error.new(Http.Error.Kind.Unknown, "private response body")
			failure.response = { code = 503, body = "private response body" }
			control.requests[1].reject(failure)
			local accepted, problem = result:await()
			expect(accepted).to.equal(false)
			expect(problem.type).to.equal(Http.Error.Kind.Unknown)
			expect(tostring(problem):find("Instance read GET /api/read", 1, true)).to.be.ok()
			for _, secret in { "private", "secret", "response body" } do
				expect(tostring(problem):find(secret, 1, true)).to.equal(nil)
			end
			expect(tostring(failure):find("private response body", 1, true)).to.be.ok()
			expect(#control.requests).to.equal(1)
			expect(#control.delays).to.equal(0)
		end)
	end)

	it("should not retry a typed write timeout", function()
		local originalPost, originalDelay = Http.post, Promise.delay
		local success, problem = pcall(function()
			local requests = 0
			Http.post = function()
				requests += 1
				return Promise.reject(Http.Error.new(Http.Error.Kind.Timeout))
			end
			Promise.delay = function()
				error("Writes must never schedule retries")
			end
			local context = ApiContext.new("http://localhost")
			context.__sessionId = "session"
			expect(context:write({ added = {}, removed = {}, updated = {} }, 0, "Filesystem"):await()).to.equal(false)
			expect(requests).to.equal(1)
		end)
		Http.post, Promise.delay = originalPost, originalDelay
		assert(success, problem)
	end)

	-- The actual HTTP adapter captures HttpService at load time. The Lune harness
	-- supplies a mutable platform boundary; Studio's native service cannot be stubbed.
	local httpService = game:GetService("HttpService")
	local adapterTest = if type(httpService) == "table" then it else itSKIP
	local function withRequestAsync(run)
		local originalRequest, originalDelay = httpService.RequestAsync, Promise.delay
		local control: any = { requests = {}, delays = {}, responses = startupResponses() }
		httpService.RequestAsync = function(_, request)
			table.insert(control.requests, request)
			return control.respond(request)
		end
		Promise.delay = function(seconds)
			return Promise.new(function(resolve)
				table.insert(control.delays, { seconds = seconds, resolve = resolve })
			end)
		end
		local success, problem = pcall(run, control)
		httpService.RequestAsync, Promise.delay = originalRequest, originalDelay
		assert(success, problem)
	end

	for _, phase in { "info", "read" } do
		local function request(context)
			context.__sessionId = "session"
			return if phase == "info" then context:connect() else context:read({ "ROOT" })
		end
		for _, code in { 403, 408, 504 } do
			adapterTest(`should not retry actual HTTP {code} startup {phase} timeout responses`, function()
				withRequestAsync(function(control)
					control.respond = function()
						return {
							StatusCode = code,
							Body = if code == 403 then "Private upstream request timed out." else "",
							Headers = {},
						}
					end
					local result = request(ApiContext.new("http://localhost"))
					-- Check before awaiting, so a wrongly scheduled backoff fails deterministically.
					expect(#control.delays).to.equal(0)
					local accepted, problem = result:await()
					expect(accepted).to.equal(false)
					expect(getmetatable(problem)).to.equal(Http.Error)
					expect(problem.response.code).to.equal(code)
					expect(tostring(problem):find("Private", 1, true)).to.equal(nil)
					expect(#control.requests).to.equal(1)
				end)
			end)
		end
		for _, failure in { "HttpError: Timedout", "HttpError: ConnectFail" } do
			adapterTest(`should retry actual RequestAsync {failure} during startup {phase}`, function()
				withRequestAsync(function(control)
					control.respond = function()
						if #control.requests == 1 then
							error(failure, 0)
						end
						return { StatusCode = 200, Body = Http.msgpackEncode(control.responses[phase]), Headers = {} }
					end
					local result = request(ApiContext.new("http://localhost"))
					expect(#control.delays).to.equal(1)
					expect(control.delays[1].seconds).to.equal(0.25)
					control.delays[1].resolve()
					expect(result:await()).to.equal(true)
					expect(#control.requests).to.equal(2)
				end)
			end)
			adapterTest(`should bound actual RequestAsync {failure} during startup {phase}`, function()
				withRequestAsync(function(control)
					control.respond = function()
						error(failure, 0)
					end
					local result = request(ApiContext.new("http://localhost"))
					for attempt = 1, 2 do
						expect(control.delays[attempt].seconds).to.equal(0.25 * attempt)
						control.delays[attempt].resolve()
					end
					local accepted, problem = result:await()
					expect(accepted).to.equal(false)
					expect(problem.response).to.equal(nil)
					expect(tostring(problem):find("3 attempts", 1, true)).to.be.ok()
					expect(#control.requests).to.equal(3)
					expect(#control.delays).to.equal(2)
				end)
			end)
		end
		adapterTest(`should not retry permanent actual RequestAsync errors during startup {phase}`, function()
			withRequestAsync(function(control)
				for _, failure in { "HTTP requests are not enabled", "Unknown failure", "arbitrary timed out text" } do
					control.respond = function()
						error(failure, 0)
					end
					local index = #control.requests + 1
					local result = request(ApiContext.new("http://localhost"))
					expect(#control.delays).to.equal(0)
					expect(result:await()).to.equal(false)
					expect(#control.requests).to.equal(index)
				end
			end)
		end)
	end

	adapterTest("should not retry an actual RequestAsync write timeout", function()
		withRequestAsync(function(control)
			control.respond = function()
				error("HttpError: Timedout", 0)
			end
			local context = ApiContext.new("http://localhost")
			expect(context:write({ added = {}, removed = {}, updated = {} }, 0, "Studio"):await()).to.equal(false)
			expect(#control.requests).to.equal(1)
			expect(control.requests[1].Method).to.equal("POST")
			expect(#control.delays).to.equal(0)
		end)
	end)

	local function writeResponse(sessionId, cursor)
		return {
			sessionId = sessionId,
			messageCursor = cursor,
			accepted = { removed_instances = {}, added_instances = {}, updated_instances = {} },
			conflicts = { removed_instances = {}, added_instances = {}, updated_instances = {} },
			errors = {},
		}
	end

	it("should keep large info and write acknowledgements synchronous by default", function()
		withStartupHttp(function(control)
			withDecodeClock(function(clock)
				local context = ApiContext.new("http://localhost")
				local info = context:connect()
				control.answer(1, "info", function(body)
					body.padding = table.create(2500, 0)
				end)
				expect(info:await()).to.equal(true)
				local originalPost = Http.post
				local success, problem = pcall(function()
					Http.post = function()
						local body = writeResponse("session", 0)
						body.padding = table.create(2500, 0)
						return Promise.resolve({ code = 200, body = Http.msgpackEncode(body) })
					end
					expect(context:write({ added = {}, removed = {}, updated = {} }, 0, "Studio"):await()).to.equal(
						true
					)
					expect(#clock.threads).to.equal(0)
				end)
				Http.post = originalPost
				assert(success, problem)
			end)
		end)
	end)

	it("should transmit private proof separately and validate capabilities even without optional checks", function()
		local Settings = require(script.Parent.Settings)
		local originalPost = Http.post
		local enabled = Settings:get("typecheckingEnabled")
		local token = "12345678-1234-4234-8234-123456789abc"
		local success, problem = pcall(function()
			Settings:set("typecheckingEnabled", false)
			for _, kind in { "valid", "legacy", "malformed", "wrong-version", "conflict", "partial" } do
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "session"
				context:setMessageCursor(2)
				Http.post = function(_, encoded)
					local sent = Http.msgpackDecode(encoded)
					expect(sent.predecessor).to.equal(token)
					expect(sent.sourceBaselines.SCRIPT).to.equal(1)
					expect(sent.operationId).to.equal("public")
					local response = writeResponse("session", 3)
					response.operationId = "public"
					response.acknowledgement = if kind == "legacy" then nil else token
					if kind == "malformed" then
						response.acknowledgement = "public"
					elseif kind == "wrong-version" then
						response.acknowledgement = "12345678-1234-1234-8234-123456789abc"
					elseif kind == "conflict" then
						response.conflicts.removed_instances = { "SCRIPT" }
					elseif kind == "partial" then
						response.errors = { { id = "SCRIPT", property = "Source", message = "Rejected." } }
					end
					return Promise.resolve({ code = 200, body = Http.msgpackEncode(response) })
				end
				local accepted = context
					:write({ added = {}, removed = {}, updated = {} }, 2, "Filesystem", "public", token, { SCRIPT = 1 })
					:await()
				expect(accepted).to.equal(kind == "valid" or kind == "legacy")
				expect(context:getMessageCursor()).to.equal(2)
			end
		end)
		Http.post = originalPost
		Settings:set("typecheckingEnabled", enabled)
		assert(success, problem)
	end)

	it("should round-trip parent updates with and without other changes", function()
		local Types = require(script.Parent.Types)
		local originalPost = Http.post
		local success, err = pcall(function()
			Http.post = function(_url, body)
				local updates = Http.msgpackDecode(body).updated
				expect(updates[1].changedParent).to.equal("PARENT")
				expect(updates[1].changedProperties).to.equal(nil)
				expect(updates[2].changedParent).to.equal("OTHER")
				expect(updates[2].changedName).to.equal("Moved")
				expect(updates[2].changedProperties.Value.String).to.equal("value")
				return Promise.resolve({
					code = 200,
					body = Http.msgpackEncode(writeResponse("session", 0)),
				})
			end
			local updates = {
				{ id = "FIRST", changedParent = "PARENT", changedProperties = {} },
				{
					id = "SECOND",
					changedParent = "OTHER",
					changedName = "Moved",
					changedProperties = { Value = { String = "value" } },
				},
			}
			for _, update in updates do
				assert(Types.ApiInstanceUpdate(Http.msgpackDecode(Http.msgpackEncode(update))))
			end
			local context = ApiContext.new("http://localhost")
			context.__sessionId = "session"
			expect(context:write({ added = {}, removed = {}, updated = updates }, nil, "Studio"):await()).to.equal(true)
		end)
		Http.post = originalPost
		assert(success, err)
	end)

	it("should bind a write acknowledgement to its exact operation", function()
		local originalPost = Http.post
		local success, problem = pcall(function()
			for _, returned in { "operation", "other", false } do
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "session"
				context:setMessageCursor(0)
				Http.post = function(_url, body)
					expect(Http.msgpackDecode(body).operationId).to.equal("operation")
					local response = writeResponse("session", 0)
					response.operationId = if returned then returned else nil
					return Promise.resolve({ code = 200, body = Http.msgpackEncode(response) })
				end
				local accepted =
					context:write({ added = {}, removed = {}, updated = {} }, 0, "Filesystem", "operation"):await()
				expect(accepted).to.equal(returned == "operation")
				expect(context.__connected).to.equal(returned == "operation")
			end
		end)
		Http.post = originalPost
		assert(success, problem)
	end)

	it("should require a durable checkpoint acknowledgement from the current session", function()
		local originalPost = Http.post
		local success, problem = pcall(function()
			for _, kind in { "valid", "missing", "other-session" } do
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "session"
				Http.post = function(url, body)
					expect(url).to.equal("http://localhost/api/source-checkpoints")
					local sent = Http.msgpackDecode(body)
					expect(sent.source).to.equal("local draft =")
					expect(sent.expectedSource).to.equal("return 'disk'")
					expect(sent.messageCursor).to.equal(7)
					expect(sent.sessionId).to.equal("session")
					return Promise.resolve({
						code = 200,
						body = Http.msgpackEncode({
							sessionId = if kind == "other-session" then "other" else "session",
							checkpointId = if kind == "missing" then nil else "checkpoint",
						}),
					})
				end
				expect(context:createSourceCheckpoint("SCRIPT", "local draft =", "return 'disk'", 7):await()).to.equal(
					kind == "valid"
				)
				expect(context.__connected).to.equal(kind == "valid")
			end
		end)
		Http.post = originalPost
		assert(success, problem)
	end)

	it("should reject recovery identifiers that could escape the endpoint path", function()
		local context = ApiContext.new("http://localhost")
		for _, identity in { "../secret", "with/slash", "", "?query", string.rep("a", 97) } do
			expect(function()
				context:readSourceCheckpoint(identity)
			end).to.throw()
		end
	end)

	it("should send the captured batch cursor instead of a newer cursor", function()
		local originalPost = Http.post
		local success, err = pcall(function()
			for _, cursor in { 0, 3 } do
				local sentCursor
				Http.post = function(_url, body)
					sentCursor = Http.msgpackDecode(body).messageCursor
					return Promise.resolve({
						code = 200,
						body = Http.msgpackEncode(writeResponse("current-session", 10)),
					})
				end
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "current-session"
				context:setMessageCursor(10)
				local accepted = context:write({ added = {}, removed = {}, updated = {} }, cursor, "Filesystem"):await()
				expect(accepted).to.equal(true)
				expect(sentCursor).to.equal(cursor)
			end
		end)
		Http.post = originalPost
		assert(success, err)
	end)

	it("should reject cursors behind any progress known when dispatching a write", function()
		local originalPost = Http.post
		local success, problem = pcall(function()
			for _, baseline in { 0, 9 } do
				for _, subscription in { 0, 7 } do
					for _, previous in { 0, 5 } do
						local minimum = math.max(baseline, subscription, previous)
						for _, cursor in { minimum - 1, minimum, minimum + 1 } do
							local context = ApiContext.new("http://localhost")
							context.__sessionId = "session"
							Http.post = function()
								return Promise.resolve({
									code = 200,
									body = Http.msgpackEncode(writeResponse("session", previous)),
								})
							end
							local patch = { added = {}, removed = {}, updated = {} }
							expect(context:write(patch, 0, "Filesystem"):await()).to.equal(true)
							context:setMessageCursor(subscription)
							Http.post = function()
								return Promise.resolve({
									code = 200,
									body = Http.msgpackEncode(writeResponse("session", cursor)),
								})
							end
							local accepted = context:write(patch, baseline, "Filesystem"):await()
							expect(accepted).to.equal(cursor >= minimum)
							expect(context.__connected).to.equal(cursor >= minimum)
						end
					end
				end
			end
		end)
		Http.post = originalPost
		assert(success, problem)
	end)

	it("should allow subscription progress to overtake an in-flight acknowledgement", function()
		local originalPost = Http.post
		local success, problem = pcall(function()
			local settle
			Http.post = function()
				return Promise.new(function(resolve)
					settle = resolve
				end)
			end
			local context = ApiContext.new("http://localhost")
			context.__sessionId = "session"
			context:setMessageCursor(10)
			local write = context:write({ added = {}, removed = {}, updated = {} }, 3, "Filesystem")
			context:setMessageCursor(20)
			settle({ code = 200, body = Http.msgpackEncode(writeResponse("session", 10)) })
			expect(write:await()).to.equal(true)
			expect(context.__connected).to.equal(true)
		end)
		Http.post = originalPost
		assert(success, problem)
	end)

	it("should send the chosen write authority and refuse unknown ones", function()
		local originalPost = Http.post
		local success, err = pcall(function()
			for _, authority in { "Filesystem", "Studio" } do
				local sentAuthority
				Http.post = function(_url, body)
					sentAuthority = Http.msgpackDecode(body).authority
					return Promise.resolve({
						code = 200,
						body = Http.msgpackEncode(writeResponse("current-session", 0)),
					})
				end
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "current-session"
				local accepted = context:write({ added = {}, removed = {}, updated = {} }, nil, authority):await()
				expect(accepted).to.equal(true)
				expect(sentAuthority).to.equal(authority)
			end
			for _, authority in { "studio", "Both", 1 } do
				local context = ApiContext.new("http://localhost")
				expect(function()
					context:write({ added = {}, removed = {}, updated = {} }, nil, authority)
				end).to.throw()
			end
			expect(function()
				ApiContext.new("http://localhost"):write({ added = {}, removed = {}, updated = {} })
			end).to.throw()
		end)
		Http.post = originalPost
		assert(success, err)
	end)

	it("should accept write responses without info-level console output", function()
		local Log = require(script.Parent.Parent.Packages.Log)
		local originalInfo = Log.info
		local originalPost = Http.post
		local success, err = pcall(function()
			Log.info = function()
				error("Write responses must not produce info-level console output")
			end
			Http.post = function()
				return Promise.resolve({
					code = 200,
					body = Http.msgpackEncode(writeResponse("current-session", 0)),
				})
			end
			local context = ApiContext.new("http://localhost")
			context.__sessionId = "current-session"
			local accepted, result =
				context:write({ added = {}, removed = {}, updated = {} }, nil, "Filesystem"):await()
			expect(accepted).to.equal(true)
			expect(result.sessionId).to.equal("current-session")
		end)
		Log.info = originalInfo
		Http.post = originalPost
		assert(success, err)
	end)

	it("should accept only write responses from the connected session", function()
		local originalPost = Http.post
		local success, err = pcall(function()
			for _, matches in { true, false } do
				local response = writeResponse(if matches then "current-session" else "previous-session", 0)
				Http.post = function()
					return Promise.resolve({ code = 200, body = Http.msgpackEncode(response) })
				end
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "current-session"
				local accepted, result =
					context:write({ added = {}, removed = {}, updated = {} }, nil, "Filesystem"):await()
				expect(accepted).to.equal(matches)
				if matches then
					expect(result.sessionId).to.equal("current-session")
				else
					expect(tostring(result):find("Server changed ID", 1, true)).to.be.ok()
					expect(context.__connected).to.equal(false)
				end
			end
		end)
		Http.post = originalPost
		assert(success, err)
	end)

	it("should require an acknowledgement cursor even when optional typechecking is disabled", function()
		local Settings = require(script.Parent.Settings)
		local originalGet = Settings.get
		local originalPost = Http.post
		local success, err = pcall(function()
			Settings.get = function(self, name)
				if name == "typecheckingEnabled" then
					return false
				end
				return originalGet(self, name)
			end
			for _, response in
				{
					{},
					{ messageCursor = "1" },
					{ messageCursor = -1 },
					{ messageCursor = 0.5 },
					{ messageCursor = 4294967296 },
					{ messageCursor = math.huge },
					{ messageCursor = 0 / 0 },
				}
			do
				local body = writeResponse("current-session", response.messageCursor)
				local requests = 0
				Http.post = function()
					requests += 1
					return Promise.resolve({ code = 200, body = Http.msgpackEncode(body) })
				end
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "current-session"
				local patch = { added = {}, removed = {}, updated = {} }
				local accepted, message = context:write(patch, 0, "Filesystem"):await()
				expect(accepted).to.equal(false)
				expect(tostring(message):find("write acknowledgement", 1, true)).to.be.ok()
				expect(context.__connected).to.equal(false)
				expect(context:write(patch, 0, "Filesystem"):await()).to.equal(false)
				expect(requests).to.equal(1)
			end
		end)
		Settings.get = originalGet
		Http.post = originalPost
		assert(success, err)
	end)

	it("should close the context when an acknowledgement cannot be decoded", function()
		local originalPost = Http.post
		local success, problem = pcall(function()
			for _, body in { string.char(0x81), string.char(0xD9) } do
				Http.post = function()
					return Promise.resolve({ code = 200, body = body })
				end
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "current-session"
				expect(context:write({ added = {}, removed = {}, updated = {} }, 0, "Filesystem"):await()).to.equal(
					false
				)
				expect(context.__connected).to.equal(false)
			end
		end)
		Http.post = originalPost
		assert(success, problem)
	end)

	it("should require every acknowledgement result field before accepting a future cursor", function()
		local originalPost = Http.post
		local success, problem = pcall(function()
			for mask = 0, 6 do
				local response = writeResponse("current-session", 100)
				for index, name in { "accepted", "conflicts", "errors" } do
					if bit32.band(mask, bit32.lshift(1, index - 1)) == 0 then
						response[name] = nil
					end
				end
				Http.post = function()
					return Promise.resolve({ code = 200, body = Http.msgpackEncode(response) })
				end
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "current-session"
				expect(context:write({ added = {}, removed = {}, updated = {} }, 0, "Filesystem"):await()).to.equal(
					false
				)
				expect(context.__connected).to.equal(false)
			end
		end)
		Http.post = originalPost
		assert(success, problem)
	end)

	it("should reject malformed nested acknowledgement results", function()
		local originalPost = Http.post
		local success, problem = pcall(function()
			for _, changes in
				{
					{ accepted = false },
					{ accepted = { removed_instances = {}, added_instances = {} } },
					{ accepted = { removed_instances = { false }, added_instances = {}, updated_instances = {} } },
					{ conflicts = { removed_instances = {}, added_instances = {}, updated_instances = { false } } },
					{ errors = { false } },
					{ errors = { { message = "No identity" } } },
					{ errors = { { id = "INSTANCE", message = false } } },
					{ errors = { { id = "INSTANCE", message = "Rejected", kind = false } } },
					{ errors = { { id = "INSTANCE", message = "Rejected", kind = "unrecognized" } } },
				}
			do
				local response = writeResponse("current-session", 100)
				for key, value in changes do
					response[key] = value
				end
				Http.post = function()
					return Promise.resolve({ code = 200, body = Http.msgpackEncode(response) })
				end
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "current-session"
				expect(context:write({ added = {}, removed = {}, updated = {} }, 0, "Filesystem"):await()).to.equal(
					false
				)
				expect(context.__connected).to.equal(false)
			end
		end)
		Http.post = originalPost
		assert(success, problem)
	end)

	it("should validate added snapshots and updated patch fields in acknowledgements", function()
		local function snapshot()
			return {
				snapshot_id = "INSTANCE",
				metadata = {
					ignore_unknown_instances = false,
					relevant_paths = {},
					context = { emit_legacy_scripts = false },
				},
				name = "Folder",
				class_name = "Folder",
				properties = {},
				children = {},
			}
		end
		local originalPost = Http.post
		local success, problem = pcall(function()
			for _, field in { "accepted", "conflicts" } do
				for index, mutate in
					{
						function() end,
						function(patch)
							patch.added_instances[1] = {}
						end,
						function(patch)
							patch.added_instances[1].parent_id = nil
						end,
						function(patch)
							patch.added_instances[1].instance = false
						end,
						function(patch)
							patch.added_instances[1].instance.snapshot_id = nil
						end,
						function(patch)
							patch.added_instances[1].instance.class_name = nil
						end,
						function(patch)
							patch.added_instances[1].instance.metadata = {}
						end,
						function(patch)
							patch.added_instances[1].instance.properties = false
						end,
						function(patch)
							patch.added_instances[1].instance.children = { {} }
						end,
						function(patch)
							patch.updated_instances[1] = {}
						end,
						function(patch)
							patch.updated_instances[1].id = nil
						end,
						function(patch)
							patch.updated_instances[1].changed_properties = nil
						end,
						function(patch)
							patch.updated_instances[1].changed_name = false
						end,
					}
				do
					local response = writeResponse("session", 1)
					local child = snapshot()
					child.snapshot_id = "CHILD"
					local instance = snapshot()
					instance.children = { child }
					response[field].added_instances = { { parent_id = "PARENT", instance = instance } }
					response[field].updated_instances = { { id = "INSTANCE", changed_properties = {} } }
					mutate(response[field])
					local body = Http.msgpackEncode(response)
					Http.post = function()
						return Promise.resolve({ code = 200, body = body })
					end
					local context = ApiContext.new("http://localhost")
					context.__sessionId = "session"
					local accepted = context:write({ added = {}, removed = {}, updated = {} }, 0, "Filesystem"):await()
					expect(accepted).to.equal(index == 1)
					expect(context.__connected).to.equal(index == 1)
				end
			end
		end)
		Http.post = originalPost
		assert(success, problem)
	end)

	it("should retain valid server-reported write errors without closing the context", function()
		local originalPost = Http.post
		local success, problem = pcall(function()
			for _, propertyError in { false, true, "deferred" } do
				local response = writeResponse("current-session", 100)
				response.errors = { { id = "INSTANCE", message = "The write was rejected." } }
				if propertyError == "deferred" then
					response.errors[1].kind = "sourceSerializationDeferred"
					response.errors[1].property = "Source"
				elseif propertyError then
					response.errors[1].property = "Transparency"
				end
				Http.post = function()
					return Promise.resolve({ code = 200, body = Http.msgpackEncode(response) })
				end
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "current-session"
				local accepted, result =
					context:write({ added = {}, removed = {}, updated = {} }, 0, "Filesystem"):await()
				expect(accepted).to.equal(true)
				expect(result.errors[1].message).to.equal("The write was rejected.")
				expect(result.errors[1].kind).to.equal(response.errors[1].kind)
				expect(context.__connected).to.equal(true)
			end
		end)
		Http.post = originalPost
		assert(success, problem)
	end)

	it("should preserve valid acknowledgement cursors at both protocol bounds", function()
		local originalPost = Http.post
		local success, err = pcall(function()
			for _, cursor in { 0, 4294967295 } do
				Http.post = function()
					return Promise.resolve({
						code = 200,
						body = Http.msgpackEncode(writeResponse("current-session", cursor)),
					})
				end
				local context = ApiContext.new("http://localhost")
				context.__sessionId = "current-session"
				local accepted, response =
					context:write({ added = {}, removed = {}, updated = {} }, 0, "Filesystem"):await()
				expect(accepted).to.equal(true)
				expect(response.messageCursor).to.equal(cursor)
			end
		end)
		Http.post = originalPost
		assert(success, err)
	end)

	-- Connects with Http.get answering the info request with the given body, and returns
	-- what the connect promise settled with.
	local function connectWithInfoBody(body)
		local originalGet = Http.get
		local results
		local success, err = pcall(function()
			Http.get = function()
				return Promise.resolve({ code = 200, body = body })
			end
			results = table.pack(ApiContext.new("http://localhost"):connect():await())
		end)
		Http.get = originalGet
		assert(success, err)
		return table.unpack(results, 1, results.n)
	end

	it("requires directional protocol thirteen while preserving ordinary protocol twelve", function()
		local HttpService = game:GetService("HttpService")
		for _, direction in { false, "fs-to-studio", "studio-to-fs", "bidirectional" } do
			for _, protocol in { 11, 12, 13, 14 } do
				for _, encoding in { "json", "msgpack" } do
					local info = {
						sessionId = "server-session",
						serverVersion = "7.7.1",
						protocolVersion = protocol,
						projectName = "Project",
						rootInstanceId = "root",
					}
					if direction then
						info.syncDirection = direction
						info.supportedSyncDirections = { "fs-to-studio", "studio-to-fs", "bidirectional" }
					end
					local body = if encoding == "json" then HttpService:JSONEncode(info) else Http.msgpackEncode(info)
					local connected, response = connectWithInfoBody(body)
					local expectedProtocol = if direction then 13 else 12
					expect(connected).to.equal(protocol == expectedProtocol)
					if connected then
						expect(response.protocolVersion).to.equal(expectedProtocol)
						expect(response.syncDirection).to.equal(if direction then direction else nil)
					else
						expect(response:find("different protocol version", 1, true)).to.be.ok()
					end
				end
			end
		end
	end)

	it("keeps directional protocol mismatches unwritable without installing a session", function()
		withStartupHttp(function(control)
			local context = ApiContext.new("http://localhost")
			local connection = context:connect()
			control.answer(1, "info", function(body)
				body.syncDirection = "studio-to-fs"
				body.protocolVersion = 12
				body.supportedSyncDirections = { "fs-to-studio", "studio-to-fs", "bidirectional" }
			end)
			expect(connection:await()).to.equal(false)
			expect(context.__sessionId).to.equal(nil)
			local originalPost = Http.post
			local writes = 0
			Http.post = function()
				writes += 1
				return Promise.reject("Unexpected write.")
			end
			local success, problem = pcall(function()
				expect(context:write({ added = {}, removed = {}, updated = {} }, nil, "Studio"):await()).to.equal(false)
				expect(writes).to.equal(0)
				expect(#control.requests).to.equal(1)
			end)
			Http.post = originalPost
			assert(success, problem)
		end)
	end)

	it("should report a protocol mismatch when the server answers with JSON", function()
		local HttpService = game:GetService("HttpService")
		local Config = require(script.Parent.Config)

		local connected, message = connectWithInfoBody(HttpService:JSONEncode({
			sessionId = "old-session",
			serverVersion = "7.5.1",
			protocolVersion = 4,
			projectName = "Old Project",
		}))

		expect(connected).to.equal(false)
		expect(message:find("different protocol version", 1, true)).to.be.ok()
		expect(message:find("Your server is version 7.5.1, with protocol version 4.", 1, true)).to.be.ok()
		expect(message:find(`with protocol version {Config.protocolVersion}.`, 1, true)).to.be.ok()
	end)

	it("should reject protocol eleven peers and require cursor-capable protocol twelve", function()
		for _, protocol in { 11, 12 } do
			local connected, body = connectWithInfoBody(Http.msgpackEncode({
				sessionId = "server-session",
				serverVersion = "7.7.1",
				protocolVersion = protocol,
				projectName = "Project",
				rootInstanceId = "root",
			}))
			expect(connected).to.equal(protocol == 12)
			if protocol == 11 then
				expect(body:find("different protocol version", 1, true)).to.be.ok()
			else
				expect(body.protocolVersion).to.equal(12)
			end
		end
	end)

	it("should report an unreadable response when the body is neither msgpack nor JSON", function()
		local connected, message = connectWithInfoBody("<html>Not a Rojo+ server</html>")

		expect(connected).to.equal(false)
		expect(message:find("couldn't read its response", 1, true)).to.be.ok()
	end)

	it("should connect when the server answers with msgpack", function()
		local Config = require(script.Parent.Config)

		local connected, body = connectWithInfoBody(Http.msgpackEncode({
			sessionId = "new-session",
			serverVersion = "7.7.1",
			protocolVersion = Config.protocolVersion,
			projectName = "New Project",
			rootInstanceId = "root",
		}))

		expect(connected).to.equal(true)
		expect(body.sessionId).to.equal("new-session")
	end)
end
