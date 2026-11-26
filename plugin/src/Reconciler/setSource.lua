--[[
	Creates a guarded Source setter and a continuous, injectable observation boundary.

	Observations are delivered immediately, including during a remote proposal. They are
	not proof of a remote commit. Only a successful editor invocation after a non-nil final
	proposal confirms that operation. A retry discards the preceding proposal's evidence.

	If a successful call returns before its change notification, its visible commit is
	observed at return. The first later delivery in the same lease is absorbed only when
	no other observation intervened and the text still equals that commit.

	Cancel revokes the observer lease immediately and marks CancellationRequested. Invoked
	operations stay pending until the engine returns, then deliver one actual terminal
	outcome to their retained completion listeners. Observe adopts that pending operation
	through Scheduled and Completed even when its Generation belongs to an older lease.
	Observation revisions stay continuous while an invocation drains across leases.

	CommitUncertain means a non-nil proposal still lacks a successful engine return. It
	clears on rejection or successful return, but stays true on a failed proposed call.
	ConsumerErrors records protected callback failures, also reported asynchronously as
	errors. They do not rewrite the engine outcome or block another request's runner.
]]

local sourceEdit = require("./sourceEdit")

local function createSourceSetter(boundary)
	assert(type(boundary) == "table", "Editor boundary must be a table.")
	assert(type(boundary.ConnectChanged) == "function", "ConnectChanged must be a function.")
	assert(type(boundary.GetEditorSource) == "function", "GetEditorSource must be a function.")
	assert(type(boundary.Spawn) == "function", "Spawn must be a function.")
	assert(type(boundary.UpdateSourceAsync) == "function", "UpdateSourceAsync must be a function.")

	local states = {}
	local generation = 0
	local setter = {}

	local function reportError(message)
		-- Report outside the engine callback and runner so consumer failures cannot be
		-- mistaken for editor failures or prevent another request from draining.
		boundary.Spawn(function()
			error(message, 0)
		end)
	end

	local function notify(request, kind, callback, value)
		if callback == nil then
			return
		end
		local success, err = pcall(callback, value)
		if not success then
			local message = `Source {kind} consumer failed. {tostring(err)}`
			if request and request.Error then
				message ..= ` Source operation failed. {request.Error}`
			end
			if request then
				request.ConsumerErrors = request.ConsumerErrors or {}
				table.insert(request.ConsumerErrors, message)
			end
			reportError(message)
		end
	end

	local function complete(request, status, err)
		if request == nil or request.Status ~= "pending" then
			return
		end
		request.Status = status
		request.Error = err
		local observers, callback = request.Observers, request.Callback
		-- Detach before calling consumers. They may cancel, replace, or enqueue work.
		request.Observers, request.Callback = nil, nil
		for _, observer in observers do
			notify(request, "completion", observer.Completed, request)
		end
		notify(request, "completion", callback, request)
	end

	local function release(instance, state)
		if states[instance] == state then
			states[instance] = nil
		end
		state.Connection:Disconnect()
		state.Changes:Disconnect()
	end

	function setter.Cancel(instance, reason)
		local state = states[instance]
		if state == nil then
			return
		end
		local request, active = state.Request, state.Active
		-- Revoke the session lease before notifying anyone. Keep the instance's runner,
		-- revision stream, and active proposal alive until the engine invocation returns.
		state.Generation, state.Request, state.OwedDelivery = nil, nil, nil
		table.clear(state.Observers)
		if request and request ~= active then
			request.CancellationRequested = true
			request.Reason = reason or "untracked"
		end
		if active and active.Status == "pending" then
			active.CancellationRequested = true
			active.Reason = active.Reason or reason or "untracked"
			-- Cancellation can invalidate retries, not revoke an engine-owned proposal.
			active.CommitUncertain = active.ProposedRevision ~= nil
		end
		if not state.Running then
			release(instance, state)
		end
		complete(if request ~= active then request else nil, "locally-cancelled")
	end

	local function observe(instance, state, owesDelivery)
		if states[instance] ~= state then
			return
		end
		local readSuccess, value = pcall(boundary.GetEditorSource, instance)
		local owed = state.OwedDelivery
		if
			not owesDelivery
			and readSuccess
			and owed
			and owed.Generation == state.Generation
			and owed.Revision == state.Revision
			and owed.Value == value
		then
			-- Deferred engine delivery arrives after its successful call returned, and
			-- reads the latest text. Each such call already produced its observation, so
			-- absorb one matching delivery per owed call while nothing else intervened.
			owed.Count -= 1
			if owed.Count == 0 then
				state.OwedDelivery = nil
			end
			return
		end
		state.Revision += 1
		local active = state.Active
		local candidate
		if readSuccess and active and active.ProposedRevision == state.Revision - 1 and active.Value == value then
			candidate = active
			active.ObservedRevision = state.Revision
		end
		if candidate and owesDelivery then
			-- Consecutive owed commits accumulate. Anything else revokes every debt.
			local count = if owed
					and owed.Generation == state.Generation
					and owed.Revision == state.Revision - 1
				then owed.Count
				else 0
			state.OwedDelivery =
				{ Count = count + 1, Generation = state.Generation, Revision = state.Revision, Value = value }
		else
			state.OwedDelivery = nil
		end
		local observation = {
			Error = if readSuccess then nil else tostring(value),
			Generation = state.Generation,
			Operation = candidate,
			Revision = state.Revision,
			Value = if readSuccess then value else nil,
		}
		-- Apart from one owed delivery, never suppress typing by content, open-document
		-- status, or heartbeat pause.
		for _, observer in table.clone(state.Observers) do
			notify(candidate, "change", observer.Changed, observation)
		end
	end

	local function getState(instance)
		local state = states[instance]
		if state then
			if state.Generation == nil then
				generation += 1
				state.Generation = generation
			end
			return state
		end
		generation += 1
		state = { Generation = generation, Revision = 0, Observers = {} }
		states[instance] = state
		state.Changes = boundary.ConnectChanged(instance, function()
			observe(instance, state)
		end)
		state.Connection = instance.Destroying:Connect(function()
			if states[instance] == state then
				setter.Cancel(instance, "destroyed")
			end
		end)
		return state
	end

	function setter.Observe(instance, changed, completed, scheduled)
		assert(type(changed) == "function", "Source observer must be a function.")
		assert(completed == nil or type(completed) == "function", "Completion must be a function.")
		assert(scheduled == nil or type(scheduled) == "function", "Scheduling observer must be a function.")
		local previous = states[instance]
		if previous and next(previous.Observers) ~= nil then
			-- Reconnection can adopt the same instance before the old session stops.
			-- Its callbacks and queued requests must not inherit the new owner's lease.
			local previousGeneration = previous.Generation
			setter.Cancel(instance, "superseded-session")
			local replacement = states[instance]
			if replacement and next(replacement.Observers) ~= nil then
				-- Cancellation delivery can reenter Observe. That newer owner wins; the
				-- interrupted adoption returns a stale lease, just like a stopped adoption.
				return { Generation = previousGeneration, Disconnect = function() end }
			end
		end
		local state = getState(instance)
		local connection = { Generation = state.Generation }
		local observer = { Changed = changed, Completed = completed, Scheduled = scheduled }
		state.Observers[connection] = observer
		function connection.Disconnect(_, reason)
			if states[instance] == state and state.Generation == connection.Generation then
				setter.Cancel(instance, reason)
			end
			state.Observers[connection] = nil
		end
		-- A new session adopts an uncertain old operation as well as its final outcome.
		-- Its observations remain provisional until that invocation really returns.
		local pending = { state.Active, state.Request }
		local seen = {}
		for _, request in pending do
			if state.Generation ~= connection.Generation then
				break
			end
			if request.Status == "pending" and not seen[request] then
				seen[request] = true
				request.Observers[connection] = observer
				notify(request, "scheduling", scheduled, request)
			end
		end
		return connection
	end

	function setter.GetRevision(instance)
		local state = states[instance]
		return if state and state.Generation then state.Revision else nil, if state then state.Generation else nil
	end

	local function drain(instance, state)
		while states[instance] == state and state.Request do
			local request = state.Request
			state.Active = request
			local appliedFrom, appliedRevision
			local success, err = pcall(function()
				boundary.UpdateSourceAsync(instance, function()
					-- A rejected proposal (including matching user text) is not a commit.
					appliedFrom, appliedRevision = nil, nil
					request.ProposedRevision, request.ObservedRevision = nil, nil
					request.CommitUncertain = false
					if
						states[instance] ~= state
						or state.Request ~= request
						or state.Generation ~= request.Generation
						or request.CancellationRequested
					then
						return nil
					end
					local actual = boundary.GetEditorSource(instance)
					if actual ~= request.Expected or state.Revision ~= request.ExpectedRevision then
						return nil
					end
					appliedFrom, appliedRevision = actual, state.Revision
					request.ProposedRevision = state.Revision
					request.CommitUncertain = true
					return request.Value
				end)
			end)

			request.CommitUncertain = not success and request.ProposedRevision ~= nil
			if
				success
				and appliedFrom ~= nil
				and appliedFrom ~= request.Value
				and request.ObservedRevision == nil
				and state.Revision == appliedRevision
			then
				local readSuccess, visible = pcall(boundary.GetEditorSource, instance)
				if readSuccess and visible == request.Value then
					-- Deferred signals can deliver after the call returns. Observe the visible
					-- commit while this operation is still active, then owe one delivery.
					observe(instance, state, true)
				end
			end
			state.Active = nil
			if state.Request == request then
				state.Request = nil
			end

			local status
			if not success then
				status = "failed"
			elseif appliedFrom ~= nil then
				status = "committed"
				request.CommittedRevision = request.ObservedRevision
				request.NoChange = appliedFrom == request.Value
				if request.NoChange then
					request.CommittedRevision = nil
				end
				local nextRequest = state.Request
				local readSuccess, visible
				if nextRequest then
					readSuccess, visible = pcall(boundary.GetEditorSource, instance)
				end
				if
					nextRequest
					and nextRequest.Expected == appliedFrom
					and nextRequest.ExpectedRevision == appliedRevision
					and state.Revision == (if request.NoChange then appliedRevision else request.CommittedRevision)
					and readSuccess
					and visible == request.Value
				then
					-- Advance only from a confirmed, still-visible preceding commit.
					nextRequest.Expected = request.Value
					nextRequest.ExpectedRevision = state.Revision
				end
			elseif request.CancellationRequested then
				status = "locally-cancelled"
			elseif state.Request ~= nil then
				status = "superseded"
			else
				status = "locally-cancelled"
				request.Reason = "local-edit"
			end
			local handled = request.Callback ~= nil
			for _, observer in request.Observers do
				handled = handled or observer.Completed ~= nil
			end
			complete(request, status, if success then nil else tostring(err))
			if not success and not handled then
				error(err, 0)
			end
		end
	end

	local start
	local function run(instance, state)
		local success, err = pcall(drain, instance, state)
		-- Finally-equivalent cleanup also covers unexpected runner failures. Consumers
		-- may have queued a successor or replaced the lease during terminal delivery.
		if not success then
			local active = state.Active
			state.Active = nil
			if state.Request == active then
				state.Request = nil
			end
			complete(active, "failed", tostring(err))
		end
		state.Running = false
		if states[instance] == state then
			if state.Request then
				start(instance, state)
			elseif next(state.Observers) == nil then
				release(instance, state)
			end
		end
		if not success then
			error(`Source runner failed. {tostring(err)}`, 0)
		end
	end

	function start(instance, state)
		if states[instance] == state and not state.Running and state.Request ~= nil then
			state.Running = true
			-- No per-script wait in the caller, including during large initial syncs.
			boundary.Spawn(function()
				run(instance, state)
			end)
		end
	end

	local function set(instance, value, completed, serverOperation)
		assert(type(value) == "string", "Source must be a string.")
		assert(completed == nil or type(completed) == "function", "Completion must be a function.")
		local expected = boundary.GetEditorSource(instance)
		local state = getState(instance)
		local request = {
			Callback = completed,
			CommitUncertain = false,
			Observers = table.clone(state.Observers),
			Expected = expected,
			ExpectedRevision = state.Revision,
			Generation = state.Generation,
			ServerOperation = serverOperation,
			Status = "pending",
			Value = value,
		}
		local previous = state.Request
		state.Request = request
		if previous ~= state.Active then
			complete(previous, "superseded")
		end
		if states[instance] ~= state or request.Status ~= "pending" then
			return request
		end
		for _, observer in table.clone(state.Observers) do
			notify(request, "scheduling", observer.Scheduled, request)
		end
		if states[instance] ~= state or request.Status ~= "pending" then
			return request
		end
		if previous == nil and not state.Running and expected == value then
			state.Request = nil
			request.NoChange = true
			complete(request, "committed")
			if states[instance] == state and not state.Running and next(state.Observers) == nil then
				release(instance, state)
			end
			return request
		end
		start(instance, state)
		return request
	end

	return setmetatable(setter, {
		__call = function(_, ...)
			return set(...)
		end,
	})
end

-- One dispatcher serves both continuous map observers and standalone reification writes.
-- It is lazy so the deterministic setter can run without any Studio services.
local function fromService(service, spawn)
	assert(type(spawn) == "function", "Source scheduler must be a function.")
	local observers = {}
	local count = 0
	local documents
	return createSourceSetter({
		ConnectChanged = function(instance, changed)
			observers[instance] = changed
			count += 1
			if documents == nil then
				documents = service.TextDocumentDidChange:Connect(function(document)
					local observer = observers[document:GetScript()]
					if observer then
						observer()
					end
				end)
			end
			local source = instance:GetPropertyChangedSignal("Source"):Connect(function()
				if service:FindScriptDocument(instance) == nil then
					changed()
				end
			end)
			return {
				Disconnect = function()
					source:Disconnect()
					if observers[instance] == changed then
						observers[instance] = nil
						count -= 1
						if count == 0 then
							documents:Disconnect()
							documents = nil
						end
					end
				end,
			}
		end,
		GetEditorSource = function(instance)
			return service:GetEditorSource(instance)
		end,
		Spawn = spawn,
		UpdateSourceAsync = function(instance, callback)
			local rejectedDocument, rejectedText, rejection
			while true do
				local document = service:FindScriptDocument(instance)
				if document == nil then
					return service:UpdateSourceAsync(instance, callback)
				end
				local current = document:GetText()
				local proposed = callback(current)
				if proposed == nil then
					return
				end
				-- A retry always passes through the revision guard first. An unchanged
				-- rejected state is an editor failure, not permission to spin or overwrite.
				if document == rejectedDocument and current == rejectedText then
					error(rejection, 0)
				end
				local edit = sourceEdit(current, proposed)
				if edit == nil then
					return
				end
				local invoked, accepted, err = pcall(
					document.EditTextAsync,
					document,
					edit.Text,
					edit.StartLine,
					edit.StartCharacter,
					edit.EndLine,
					edit.EndCharacter
				)
				if not invoked then
					-- A thrown call has no negative acknowledgement. Retain its uncertain
					-- proposal instead of cancelling evidence after a possible commit.
					error(accepted, 0)
				end
				if accepted then
					return
				end
				rejectedDocument, rejectedText, rejection = document, current, err
			end
		end,
	})
end

local sharedSetter
local function shared()
	if sharedSetter == nil then
		sharedSetter = fromService(game:GetService("ScriptEditorService"), task.spawn)
	end
	return sharedSetter
end

return setmetatable({ fromService = fromService, shared = shared }, {
	__call = function(_, boundary)
		return createSourceSetter(boundary)
	end,
})
