return function()
	local ServeSession = require(script.Parent.ServeSession)
	local Promise = require(script.Parent.Parent.Packages.Promise)

	local function writeResult(errors, conflicts)
		return {
			errors = errors or {},
			conflicts = conflicts or {
				removed_instances = {},
				added_instances = {},
				updated_instances = {},
			},
		}
	end

	it("should report transport failures to the notification callback", function()
		local session = setmetatable({
			__apiContext = {
				write = function()
					return Promise.reject("Connection lost")
				end,
			},
			__syncResults = {},
		}, ServeSession)
		local notification = nil
		session:setSyncResultCallback(function(status, result)
			notification = { status = status, result = result }
		end)
		session:__writePatch({ updated = {}, added = {}, removed = {} }):await()
		local results = session:getSyncResults()
		expect(#results).to.equal(1)
		expect(results[1].status).to.equal("Failed")
		expect(notification.status).to.equal("Failed")
		expect(notification.result.errors[1].message).to.equal("Connection lost")
	end)

	it("should keep one record per write that moves from pending to its outcome", function()
		local resolveWrite = nil
		local session = setmetatable({
			__apiContext = {
				write = function()
					return Promise.new(function(resolve)
						resolveWrite = resolve
					end)
				end,
			},
			__syncResults = {},
		}, ServeSession)
		local patch = { updated = {}, added = {}, removed = {} }
		local write = session:__writePatch(patch)

		local pending = session:getSyncResults()
		expect(#pending).to.equal(1)
		expect(pending[1].status).to.equal("Pending")
		expect(pending[1].patch).to.equal(patch)

		resolveWrite({
			errors = {},
			conflicts = { removed_instances = {}, added_instances = {}, updated_instances = {} },
		})
		write:await()

		local finished = session:getSyncResults()
		expect(#finished).to.equal(1)
		expect(finished[1].status).to.equal("Saved")
		expect(finished[1].patch).to.equal(patch)
		-- Callers get copies, so earlier snapshots don't change underneath them.
		expect(pending[1].status).to.equal("Pending")
	end)

	it("should retain the first local change cursor until the batch is flushed", function()
		local cursor = 0
		local batcher = {
			__pendingPropertyChanges = {},
			add = function(self, instance, propertyName)
				self.__pendingPropertyChanges[instance] = propertyName
			end,
		}
		local session = setmetatable({
			__apiContext = {
				getMessageCursor = function()
					return cursor
				end,
			},
			__changeBatcher = batcher,
		}, ServeSession)
		session:__queueChange("Part", "Transparency")
		cursor = 5
		session:__queueChange("Part", "CanCollide")
		expect(session.__pendingWriteCursor).to.equal(0)
		table.clear(batcher.__pendingPropertyChanges)
		session:__queueChange("Part", "Transparency")
		expect(session.__pendingWriteCursor).to.equal(5)
	end)

	it("should classify saved writes", function()
		expect(ServeSession.getWriteStatus(writeResult())).to.equal("Saved")
	end)

	it("should classify every conflict shape", function()
		for _, key in { "removed_instances", "added_instances", "updated_instances" } do
			local result = writeResult()
			table.insert(result.conflicts[key], {})
			expect(ServeSession.getWriteStatus(result)).to.equal("Conflicted")
		end
	end)

	it("should classify errors as failed even with conflicts", function()
		local result = writeResult({ { message = "Write failed" } })
		table.insert(result.conflicts.added_instances, {})
		expect(ServeSession.getWriteStatus(result)).to.equal("Failed")
	end)
end
