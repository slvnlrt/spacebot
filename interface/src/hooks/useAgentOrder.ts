import { useState, useEffect } from "react";

const STORAGE_KEY = "spacebot:agent-order";

/**
 * Hook to manage persistent agent ordering via localStorage.
 * Preserves user's custom sort order across sessions.
 */
export function useAgentOrder(agentIds: string[]) {
	const [order, setOrder] = useState<string[]>([]);

	// Load order from localStorage on mount and when agentIds change
	useEffect(() => {
		const stored = localStorage.getItem(STORAGE_KEY);
		let storedOrder: string[] = [];
		
		if (stored) {
			try {
				storedOrder = JSON.parse(stored);
			} catch {
				storedOrder = [];
			}
		}

		// Merge: keep stored order for existing agents, append new agents
		const storedSet = new Set(storedOrder);
		const newAgents = agentIds.filter((id) => !storedSet.has(id));
		const validStoredOrder = storedOrder.filter((id) => agentIds.includes(id));
		
		setOrder([...validStoredOrder, ...newAgents]);
	}, [agentIds]);

	// Persist order to localStorage
	const updateOrder = (newOrder: string[]) => {
		setOrder(newOrder);
		localStorage.setItem(STORAGE_KEY, JSON.stringify(newOrder));
	};

	return [order, updateOrder] as const;
}
