return function()
	local ApiContext = require(script.Parent.ApiContext)
	local Http = require(script.Parent.Parent.Packages.Http)
	local Promise = require(script.Parent.Parent.Packages.Promise)

	local function writeResponse(sessionId, cursor)
		return {
			sessionId = sessionId,
			messageCursor = cursor,
			accepted = { removed_instances = {}, added_instances = {}, updated_instances = {} },
			conflicts = { removed_instances = {}, added_instances = {}, updated_instances = {} },
			errors = {},
		}
	end

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
