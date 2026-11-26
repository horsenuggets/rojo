--[[
	Defines the process of "hydration" -- matching up a virtual DOM with
	concrete instances and assigning them IDs.
]]

local Packages = script.Parent.Parent.Parent.Packages
local Log = require(Packages.Log)

local invariant = require(script.Parent.Parent.invariant)
local countMatchingProperties = require(script.Parent.countMatchingProperties)
local services = require(script.Parent.services)

-- When several existing children share a Name and ClassName we disambiguate
-- them by scoring how well each one's properties match the virtual instance.
-- That scoring is far more expensive than a Name/ClassName check, so we only do
-- it for reasonably-sized groups. Larger groups (e.g. a folder with thousands of
-- identically-named parts) fall back to the original order-based matching, which
-- bounds the added work to roughly MAX_CANDIDATES_TO_SCORE^2 property reads per
-- group regardless of how large the group is.
local MAX_CANDIDATES_TO_SCORE = 32

local function structuralProfiles(bucket, virtualInstances, step)
	if bucket.structuralPrepared then
		return bucket.structuralProfiles
	end
	bucket.structuralPrepared = true

	local virtualIds = bucket.virtualIds
	if virtualIds == nil or #virtualIds ~= #bucket.instances then
		return nil
	end

	-- Unknown children are not identity evidence. Intern only name/class pairs
	-- explicitly described in this sibling group, including non-archivable
	-- children that hydration can still match.
	local relevant = {}
	local keyCount = 0
	for _, id in virtualIds do
		step()
		for _, childId in virtualInstances[id].Children do
			step()
			local child = virtualInstances[childId]
			local classes = relevant[child.Name]
			if classes == nil then
				classes = {}
				relevant[child.Name] = classes
			end
			if classes[child.ClassName] == nil then
				keyCount += 1
				classes[child.ClassName] = keyCount
			end
		end
	end
	if keyCount == 0 then
		return nil
	end

	-- There are at most 32 virtual profiles. Sparse exact comparisons avoid
	-- recursive fingerprints, sorting, and dense group-wide child vectors.
	local profiles = {}
	local function matchingProfile(candidate)
		for id, profile in profiles do
			step()
			if profile.total == candidate.total and profile.size == candidate.size then
				local equal = true
				for key, count in candidate.counts do
					step()
					if profile.counts[key] ~= count then
						equal = false
						break
					end
				end
				if equal then
					return id
				end
			end
		end
		return nil
	end

	local virtualProfiles = {}
	local histogram = {}
	for _, id in virtualIds do
		step()
		local candidate = { counts = {}, total = 0, size = 0 }
		for _, childId in virtualInstances[id].Children do
			step()
			local child = virtualInstances[childId]
			local key = relevant[child.Name][child.ClassName]
			if candidate.counts[key] == nil then
				candidate.size += 1
			end
			candidate.counts[key] = (candidate.counts[key] or 0) + 1
			candidate.total += 1
		end
		local profileId = matchingProfile(candidate)
		if profileId == nil then
			table.insert(profiles, candidate)
			profileId = #profiles
		end
		virtualProfiles[id] = profileId
		histogram[profileId] = (histogram[profileId] or 0) + 1
	end

	local nativeProfiles = {}
	for _, instance in bucket.instances do
		step()
		local success, children = pcall(function()
			return instance:GetChildren()
		end)
		if not success then
			return nil
		end
		local candidate = { counts = {}, total = 0, size = 0 }
		for _, child in children do
			step()
			local accessible, name, className = pcall(function()
				return child.Name, child.ClassName
			end)
			if not accessible then
				return nil
			end
			local classes = relevant[name]
			local key = classes and classes[className]
			if key then
				if candidate.counts[key] == nil then
					candidate.size += 1
				end
				candidate.counts[key] = (candidate.counts[key] or 0) + 1
				candidate.total += 1
			end
		end
		local profileId = matchingProfile(candidate)
		if profileId == nil or histogram[profileId] == 0 then
			return nil
		end
		nativeProfiles[instance] = profileId
		histogram[profileId] -= 1
	end

	-- Extra or missing relevant children make the entire structural hint
	-- ambiguous. Keep the original order fallback unless every sibling fits.
	local result = { native = nativeProfiles, virtual = virtualProfiles }
	bucket.structuralProfiles = result
	return result
end

local function hydrateInner(stats, instanceMap, virtualInstances, rootId, rootInstance, step)
	local virtualInstance = virtualInstances[rootId]

	if virtualInstance == nil then
		invariant("Cannot hydrate an instance not present in virtualInstances\nID: {}", rootId)
	end

	step()
	instanceMap:insert(rootId, rootInstance)
	stats.hydrated += 1

	local existingChildren = rootInstance:GetChildren()

	-- Group existing children by Name then ClassName so each virtual child can
	-- find its candidate matches without scanning every sibling. This is what
	-- keeps hydration fast for parents with thousands of children. Nesting the
	-- two tables (rather than a combined key) keeps the Name and ClassName checks
	-- exact, with no way for one to bleed into the other.
	local buckets = {}
	local structuralBucketCount = 0
	for _, childInstance in existingChildren do
		-- Children that match nothing never reach the recursive call, so a
		-- wide folder of them needs its own chances to yield.
		step()

		-- We guard accessing Name and ClassName in order to avoid tripping over
		-- children of DataModel that Rojo won't have permissions to access at all.
		local accessSuccess, name, className = pcall(function()
			return childInstance.Name, childInstance.ClassName
		end)
		if not accessSuccess then
			continue
		end

		local bucketsByClassName = buckets[name]
		if bucketsByClassName == nil then
			bucketsByClassName = {}
			buckets[name] = bucketsByClassName
		end

		local bucket = bucketsByClassName[className]
		if bucket == nil then
			bucket = { cursor = 1, instances = {} }
			bucketsByClassName[className] = bucket
		end

		table.insert(bucket.instances, childInstance)
		if #bucket.instances == 2 then
			bucket.virtualIds = {}
			structuralBucketCount += 1
		elseif #bucket.instances == MAX_CANDIDATES_TO_SCORE + 1 then
			bucket.virtualIds = nil
			structuralBucketCount -= 1
		end
	end

	if structuralBucketCount > 0 then
		for _, childId in virtualInstance.Children do
			step()
			local child = virtualInstances[childId]
			local classes = buckets[child.Name]
			local bucket = classes and classes[child.ClassName]
			local ids = bucket and bucket.virtualIds
			-- One extra ID is enough to reject a cardinality mismatch, without
			-- retaining an unbounded virtual group for a small native bucket.
			if ids and #ids <= #bucket.instances then
				table.insert(ids, childId)
			end
		end
	end

	-- Tracks which existing children have already been paired, so one instance
	-- isn't matched to two different virtual instances.
	local visited = {}

	for _, childId in ipairs(virtualInstance.Children) do
		step()
		local virtualChild = virtualInstances[childId]

		local bucketsByClassName = buckets[virtualChild.Name]
		local bucket = bucketsByClassName and bucketsByClassName[virtualChild.ClassName]
		if bucket == nil then
			local service = services.getService(rootInstance, virtualChild.ClassName)
			if service ~= nil and not visited[service] then
				visited[service] = true
				hydrateInner(stats, instanceMap, virtualInstances, childId, service, step)
				continue
			end

			-- No existing instance matches; diff will mark this id for creation.
			Log.trace(
				"hydrate: no existing instance matches {} ({}) for id {}",
				virtualChild.Name,
				virtualChild.ClassName,
				childId
			)
			continue
		end

		local instances = bucket.instances

		-- Advance past any leading children that have already been paired. The
		-- cursor makes order-based matching amortized O(1) per child even for
		-- very large groups, rather than rescanning the visited prefix.
		while bucket.cursor <= #instances and visited[instances[bucket.cursor]] do
			bucket.cursor += 1
		end
		if bucket.cursor > #instances then
			-- Every matching instance has already been paired with an earlier id.
			Log.trace(
				"hydrate: no unpaired instance left for {} ({}) for id {}",
				virtualChild.Name,
				virtualChild.ClassName,
				childId
			)
			continue
		end

		-- The cursor points at the earliest unvisited child, so the slots from
		-- here to the end bound how many candidates remain. Visited children
		-- after the cursor (gaps) only appear once a group is small enough to be
		-- scored -- the order-based path below always takes the earliest, which
		-- keeps the visited region a contiguous prefix. So whenever this count
		-- exceeds the cap it is exact, and we can pick the earliest match without
		-- collecting anything.
		local remaining = #instances - bucket.cursor + 1

		local match
		if remaining > MAX_CANDIDATES_TO_SCORE then
			-- Too many to score affordably; take the earliest in child order,
			-- reproducing the original Name + ClassName behavior.
			match = instances[bucket.cursor]
			Log.trace(
				"hydrate: {} candidates named {} ({}) exceeds the scoring cap of {}; matching id {} by child order",
				remaining,
				virtualChild.Name,
				virtualChild.ClassName,
				MAX_CANDIDATES_TO_SCORE,
				childId
			)
		else
			-- Collect the (at most `remaining`) unvisited candidates.
			local candidates = {}
			for index = bucket.cursor, #instances do
				local childInstance = instances[index]
				if not visited[childInstance] then
					table.insert(candidates, childInstance)
				end
			end

			if #candidates == 1 then
				-- Only one candidate, so there's nothing to disambiguate.
				match = candidates[1]
			else
				-- Properties remain the primary discriminator. Equal-property
				-- siblings can still own different subtrees; pairing those by
				-- order would make the initial diff replace or duplicate children.
				local bestScore = -1
				local bestCandidates = {}
				for _, childInstance in candidates do
					step()
					local score = countMatchingProperties(childInstance, virtualChild, instanceMap)
					if score > bestScore then
						bestScore = score
						table.clear(bestCandidates)
					end
					if score == bestScore then
						table.insert(bestCandidates, childInstance)
					end
				end
				match = bestCandidates[1]
				if #bestCandidates > 1 then
					local profiles = structuralProfiles(bucket, virtualInstances, step)
					if profiles then
						for _, childInstance in bestCandidates do
							step()
							if profiles.native[childInstance] == profiles.virtual[childId] then
								match = childInstance
								break
							end
						end
					end
				end

				stats.ambiguousGroups += 1
				stats.candidatesScored += #candidates
				Log.trace(
					"hydrate: disambiguated {} candidates named {} ({}) for id {} by property match (best score {})",
					#candidates,
					virtualChild.Name,
					virtualChild.ClassName,
					childId,
					bestScore
				)
			end
		end

		visited[match] = true
		hydrateInner(stats, instanceMap, virtualInstances, childId, match, step)
	end
end

local function noStep() end

-- `step` is called for every instance the walk touches, matched or not, so a
-- caller walking a large place can yield along the way. Without one, hydration
-- never yields.
local function hydrate(instanceMap, virtualInstances, rootId, rootInstance, step)
	-- Tallies of the work hydration did, surfaced in a single debug log below so
	-- the cost of property-based disambiguation is visible without per-node spam.
	local stats = {
		hydrated = 0,
		ambiguousGroups = 0,
		candidatesScored = 0,
	}

	hydrateInner(stats, instanceMap, virtualInstances, rootId, rootInstance, step or noStep)

	Log.debug(
		"Hydrated {} instances ({} ambiguous name+class groups, {} candidates scored)",
		stats.hydrated,
		stats.ambiguousGroups,
		stats.candidatesScored
	)
end

return hydrate
